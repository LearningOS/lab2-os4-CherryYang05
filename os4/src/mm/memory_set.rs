//! Implementation of [`MapArea`] and [`MemorySet`].

use super::{frame_alloc, FrameTracker};
use super::{PTEFlags, PageTable, PageTableEntry};
use super::{PhysAddr, PhysPageNum, VirtAddr, VirtPageNum};
use super::{StepByOne, VPNRange};
use crate::config::{MEMORY_END, PAGE_SIZE, TRAMPOLINE, TRAP_CONTEXT, USER_STACK_SIZE};
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;
use lazy_static::*;
use riscv::register::satp;
use spin::Mutex;

extern "C" {
    fn stext();
    fn etext();
    fn srodata();
    fn erodata();
    fn sdata();
    fn edata();
    fn sbss_with_stack();
    fn ebss();
    fn ekernel();
    fn strampoline();
}

lazy_static! {
    /// a memory set instance through lazy_static! managing kernel space
    pub static ref KERNEL_SPACE: Arc<Mutex<MemorySet>> =
        Arc::new(Mutex::new(MemorySet::new_kernel()));
}

/// 地址空间，控制虚拟内存空间
/// memory set structure, controls virtual-memory space
pub struct MemorySet {
    // 挂着所有多级页表的节点所在的物理页帧
    page_table: PageTable,
    // 每个 MapArea 下则挂着对应逻辑段中的数据所在的物理页帧
    areas: Vec<MapArea>,
    // 这两部分 合在一起构成了一个地址空间所需的所有物理页帧
}

/// MemorySet 实现
impl MemorySet {
    /// 新建一个空的地址空间
    pub fn new_bare() -> Self {
        Self {
            page_table: PageTable::new(),
            areas: Vec::new(),
        }
    }

    /// 
    pub fn token(&self) -> usize {
        self.page_table.token()
    }

    /// push 方法可以在当前地址空间插入一个新的逻辑段 map_area
    fn push(&mut self, mut map_area: MapArea, data: Option<&[u8]>) {
        map_area.map(&mut self.page_table);
        if let Some(data) = data {
            map_area.copy_data(&mut self.page_table, data);
        }
        self.areas.push(map_area);
    }

    /// 可以在当前地址空间插入一个 Framed 方式映射到物理内存的逻辑段
    /// 
    /// 要保证同一地址空间内的任意两个逻辑段不能存在交集
    pub fn insert_framed_area(
        &mut self,
        start_va: VirtAddr,
        end_va: VirtAddr,
        permission: MapPermission,
    ) {
        self.push(
            MapArea::new(start_va, end_va, MapType::Framed, permission),
            None,
        );
    }

    
    /// 内核空间跳板
    /// Mention that trampoline is not collected by areas.
    fn map_trampoline(&mut self) {
        self.page_table.map(
            VirtAddr::from(TRAMPOLINE).into(),
            PhysAddr::from(strampoline as usize).into(),
            PTEFlags::R | PTEFlags::X,
        );
    }
    /// Without kernel stacks.
    pub fn new_kernel() -> Self {
        let mut memory_set = Self::new_bare();
        // map trampoline
        memory_set.map_trampoline();
        // map kernel sections
        info!(".text [{:#x}, {:#x})", stext as usize, etext as usize);
        info!(".rodata [{:#x}, {:#x})", srodata as usize, erodata as usize);
        info!(".data [{:#x}, {:#x})", sdata as usize, edata as usize);
        info!(
            ".bss [{:#x}, {:#x})",
            sbss_with_stack as usize, ebss as usize
        );


        info!("mapping .text section");
        // 映射内核中的代码段(.text)
        memory_set.push(
            MapArea::new(
                (stext as usize).into(),
                (etext as usize).into(),
                MapType::Identical,
                MapPermission::R | MapPermission::X,
            ),
            None,
        );

        info!("mapping .rodata section");
        // 映射内核中的只读数据段(.rodata)
        memory_set.push(
            MapArea::new(
                (srodata as usize).into(),
                (erodata as usize).into(),
                MapType::Identical,
                MapPermission::R,
            ),
            None,
        );

        info!("mapping .data section");
        // 映射内核中的数据段(.data)
        memory_set.push(
            MapArea::new(
                (sdata as usize).into(),
                (edata as usize).into(),
                MapType::Identical,
                MapPermission::R | MapPermission::W,
            ),
            None,
        );

        info!("mapping .bss section");
        // 映射内核中的未定义数据段(.bss)
        memory_set.push(
            MapArea::new(
                (sbss_with_stack as usize).into(),
                (ebss as usize).into(),
                MapType::Identical,
                MapPermission::R | MapPermission::W,
            ),
            None,
        );

        info!("mapping physical memory");
        // 映射内核中的物理页帧
        memory_set.push(
            MapArea::new(
                (ekernel as usize).into(),
                MEMORY_END.into(),
                MapType::Identical,
                MapPermission::R | MapPermission::W,
            ),
            None,
        );
        memory_set
    }

    /// 在创建应用地址空间的时候，我们需要对 get_app_data 得到的 ELF 格式数据进行解析，找到各个逻辑段所在位置和访问限制并插入进来，最终得到一个完整的应用地址空间
    /// 
    /// Include sections in elf and trampoline and TrapContext and user stack, also returns user_sp and entry point.
    pub fn from_elf(elf_data: &[u8]) -> (Self, usize, usize) {
        let mut memory_set = Self::new_bare();
        // map trampoline
        memory_set.map_trampoline();
        // map program headers of elf, with U flag
        let elf = xmas_elf::ElfFile::new(elf_data).unwrap();
        let elf_header = elf.header;
        let magic = elf_header.pt1.magic;
        // 取出 ELF 的魔数来判断它是不是一个合法的 ELF
        assert_eq!(magic, [0x7f, 0x45, 0x4c, 0x46], "invalid elf!");
        let ph_count = elf_header.pt2.ph_count();
        let mut max_end_vpn = VirtPageNum(0);
        // 然后遍历所有的 program header 并将合适的区域加入到应用地址空间中
        for i in 0..ph_count {
            let ph = elf.program_header(i).unwrap();
            // 确认 program header 的类型是 LOAD
            if ph.get_type().unwrap() == xmas_elf::program::Type::Load {
                // 通过 ph.virtual_addr() 和 ph.mem_size() 来计算这一区域在应用地址空间中的位置
                let start_va: VirtAddr = (ph.virtual_addr() as usize).into();
                let end_va: VirtAddr = ((ph.virtual_addr() + ph.mem_size()) as usize).into();
                let mut map_perm = MapPermission::U;
                // 通过 ph.flags() 来确认这一区域访问方式的限制并将其转换为 MapPermission 类型（注意它默认包含 U 标志位）
                let ph_flags = ph.flags();
                if ph_flags.is_read() {
                    map_perm |= MapPermission::R;
                }
                if ph_flags.is_write() {
                    map_perm |= MapPermission::W;
                }
                if ph_flags.is_execute() {
                    map_perm |= MapPermission::X;
                }
                // 创建逻辑段 map_area 并 push 到应用地址空间，在 push 的时候我们需要完成数据拷贝
                let map_area = MapArea::new(start_va, end_va, MapType::Framed, map_perm);
                max_end_vpn = map_area.vpn_range.get_end();
                memory_set.push(
                    map_area,
                    Some(&elf.input[ph.offset() as usize..(ph.offset() + ph.file_size()) as usize]),
                );
            }
        }
        // 开始处理用户栈，注意在前面加载各个 program header 的时候，我们就已经维护了 max_end_vpn 记录目前涉及到的最大的虚拟页号，只需紧接着在它上面再放置一个保护页面和用户栈即可
        // map user stack with U flags
        let max_end_va: VirtAddr = max_end_vpn.into();
        let mut user_stack_bottom: usize = max_end_va.into();
        // guard page
        user_stack_bottom += PAGE_SIZE;
        let user_stack_top = user_stack_bottom + USER_STACK_SIZE;

        // 在应用地址空间中映射次高页面来存放 Trap 上下文
        memory_set.push(
            MapArea::new(
                user_stack_bottom.into(),
                user_stack_top.into(),
                MapType::Framed,
                MapPermission::R | MapPermission::W | MapPermission::U,
            ),
            None,
        );
        // map TrapContext
        memory_set.push(
            MapArea::new(
                TRAP_CONTEXT.into(),
                TRAMPOLINE.into(),
                MapType::Framed,
                MapPermission::R | MapPermission::W,
            ),
            None,
        );
        // 返回的时候，我们不仅返回应用地址空间 memory_set，也同时返回用户栈虚拟地址 user_stack_top 以及从解析 ELF 得到的该应用入口点地址，它们将被我们用来创建应用的任务控制块
        (
            memory_set,
            user_stack_top,
            elf.header.pt2.entry_point() as usize,
        )
    }

    /// 我们将 token 写入当前 CPU 的 satp CSR ，从这一刻开始 SV39 分页模式就被启用了，而且 MMU 会使用内核地址空间的多级页表进行地址转换
    pub fn activate(&self) {
        let satp = self.page_table.token();
        unsafe {
            satp::write(satp);
            // sfence.vma 指令将快表清空，使 MMU 不会看到快表中已经过期的键值对
            core::arch::asm!("sfence.vma");
        }
    }


    pub fn translate(&self, vpn: VirtPageNum) -> Option<PageTableEntry> {
        self.page_table.translate(vpn)
    }

    /// Lab2-os4 mmap 系统调用
    pub fn mmap(&mut self, start: usize, len: usize, port: usize) -> isize {
        let vpn_range = VPNRange::new(VirtAddr::from(start).floor(), VirtAddr::from(start + len).ceil());

        for vpn in vpn_range {
            if let Some(pte) = self.page_table.find_pte(vpn) {
                if pte.is_valid() {
                    return -1;
                }
            }
        }

        let mut map_permission = MapPermission::U;
        if (port & 1) != 0 {
            map_permission |= MapPermission::R;
        }
        if (port & 2) != 0 {
            map_permission |= MapPermission::W;
        }
        if (port & 4) != 0 {
            map_permission |= MapPermission::X;
        }
        
        println!("start_va: {:#x}, end_va: {:#x}, map_permission: {:#x}", start, start + len, map_permission);

        self.insert_framed_area(start.into(), (start + len).into(), map_permission);
        0
    }

    /// Lab2-os4 munmap 系统调用
    pub fn munmap(&mut self, start: usize, len: usize) -> isize {
        let vpn_range = VPNRange::new(VirtAddr::from(start).floor(), VirtAddr::from(start + len).ceil());

        println!("{:?}", vpn_range);
        
        for vpn in vpn_range {
            let pte = self.page_table.find_pte(vpn);
            if pte.is_none() || !pte.unwrap().is_valid() {
                return -1;
            }
        }

        for vpn in vpn_range {
            for area in &mut self.areas {
                if vpn < area.vpn_range.get_end() && vpn >= area.vpn_range.get_start() {
                    area.unmap_one(&mut self.page_table, vpn);
                }
            }
        }
        0
    }
}

/// 以逻辑段为单位描述一段连续地址的虚拟内存
/// map area structure, controls a contiguous piece of virtual memory
/// 和之前的 PageTable 一样，这也用到了 RAII 的思想，将这些物理页帧的生命周期绑定到它所在的逻辑段 MapArea 下，当逻辑段被回收之后这些之前分配的物理页帧也会自动地同时被回收
pub struct MapArea {
    vpn_range: VPNRange,
    // 保存了该逻辑段内的每个虚拟页面和它被映射到的物理页帧 FrameTracker 的一个键值对，这里物理页存放的是实际内存数据而不是中间的表项
    data_frames: BTreeMap<VirtPageNum, FrameTracker>,
    map_type: MapType,
    // 表示控制该逻辑段的访问方式，它是页表项标志位 PTEFlags 的一个子集，仅保留 U/R/W/X 四个标志位，因为其他的标志位仅与硬件的地址转换机制细节相关，这样的设计能避免引入错误的标志位
    map_permission: MapPermission,
}


impl MapArea {
    /// 新建一个逻辑段结构体，注意传入的起始/终止虚拟地址会分别被下取整/上取整为虚拟页号并传入 迭代器 vpn_range 中
    pub fn new(
        start_va: VirtAddr,
        end_va: VirtAddr,
        map_type: MapType,
        map_permission: MapPermission,
    ) -> Self {
        let start_vpn: VirtPageNum = start_va.floor();
        let end_vpn: VirtPageNum = end_va.ceil();
        Self {
            vpn_range: VPNRange::new(start_vpn, end_vpn),
            data_frames: BTreeMap::new(),
            map_type,
            map_permission,
        }
    }

    /// 实现一个虚拟页号映射到存放实际数据的物理页
    pub fn map_one(&mut self, page_table: &mut PageTable, vpn: VirtPageNum) {
        // 这个 ppn 是存放实际数据的物理页，而不是中间级页表的物理页
        let ppn: PhysPageNum;
        match self.map_type {
            // 当以恒等映射 Identical 方式映射的时候，物理页号就等于虚拟页号
            MapType::Identical => {
                ppn = PhysPageNum(vpn.0);
            }
            // 当以 Framed 方式映射的时候，需要分配一个物理页帧让当前的虚拟页面可以映射过去，此时页表项中的物理页号自然就是这个被分配的物理页帧的物理页号。此时还需要将这个物理页帧挂在逻辑段的 data_frames 字段下。
            MapType::Framed => {
                let frame = frame_alloc().unwrap();
                ppn = frame.ppn;
                self.data_frames.insert(vpn, frame);
            }
        }
        let pte_flags = PTEFlags::from_bits(self.map_permission.bits).unwrap();
        // 在这里实际创建并填写了三级页表
        page_table.map(vpn, ppn, pte_flags);
    }

    /// 删除虚拟页号到物理页的映射关系
    #[allow(unused)]
    pub fn unmap_one(&mut self, page_table: &mut PageTable, vpn: VirtPageNum) {
        #[allow(clippy::single_match)]
        match self.map_type {
            MapType::Framed => {
                self.data_frames.remove(&vpn);
            }
            _ => {}
        }
        page_table.unmap(vpn);
    }

    /// 将当前逻辑段到物理内存的映射加入传入的该逻辑段所属的地址空间的多级页表
    /// 
    /// 实现步骤是：对于每一个虚拟页号，都分配一个存放实际数据的物理页
    pub fn map(&mut self, page_table: &mut PageTable) {
        for vpn in self.vpn_range {
            self.map_one(page_table, vpn);
        }
    }

    /// 将当前逻辑段到物理内存的映射从传入的该逻辑段所属的地址空间的多级页表中删除
    #[allow(unused)]
    pub fn unmap(&mut self, page_table: &mut PageTable) {
        for vpn in self.vpn_range {
            self.unmap_one(page_table, vpn);
        }
    }

    /// 将切片 data 中的数据拷贝到当前逻辑段实际被内核放置在的各物理页帧上，从而 在地址空间中通过该逻辑段就能访问这些数据
    /// 
    /// 调用它的时候需要满足：切片 data 中的数据大小不超过当前逻辑段的 总大小，且切片中的数据会被对齐到逻辑段的开头，然后逐页拷贝到实际的物理页帧。
    /// 
    /// data: start-aligned but maybe with shorter length
    /// 
    /// assume that all frames were cleared before
    pub fn copy_data(&mut self, page_table: &mut PageTable, data: &[u8]) {
        // 保证要以 framed 方式映射
        assert_eq!(self.map_type, MapType::Framed);
        let mut start: usize = 0;
        // 获得起始逻辑页号
        let mut current_vpn = self.vpn_range.get_start();
        let len = data.len();


        loop {
            // 按照一页大小进行拷贝
            let src = &data[start..len.min(start + PAGE_SIZE)];
            // 从页表中查询该虚拟页号对应的物理页号，然后写入 data
            let dst = &mut page_table
                .translate(current_vpn)
                .unwrap()
                .ppn()
                .get_bytes_array()[..src.len()];
            dst.copy_from_slice(src);
            start += PAGE_SIZE;
            if start >= len {
                break;
            }
            // 虚拟页号加一
            current_vpn.step();
        }
    }
}

#[derive(Copy, Clone, PartialEq, Debug)]
/// MapType 描述该逻辑段内的所有虚拟页面映射到物理页帧的同一种方式，它是一个枚举类型，在内核当前的实现中支持两种方式
/// 其中 Identical 表示恒等映射，用于在启用多级页表之后仍能够访问一个特定的物理地址指向的物理内存；而 Framed 则表示对于每个虚拟页面都需要映射到一个新分配的物理页帧
pub enum MapType {
    Identical,
    Framed,
}

bitflags! {
    /// map permission corresponding to that in pte: `R W X U`
    pub struct MapPermission: u8 {
        const R = 1 << 1;
        const W = 1 << 2;
        const X = 1 << 3;
        const U = 1 << 4;
    }
}

#[allow(unused)]
pub fn remap_test() {
    let mut kernel_space = KERNEL_SPACE.lock();
    let mid_text: VirtAddr = ((stext as usize + etext as usize) / 2).into();
    let mid_rodata: VirtAddr = ((srodata as usize + erodata as usize) / 2).into();
    let mid_data: VirtAddr = ((sdata as usize + edata as usize) / 2).into();
    assert!(!kernel_space
        .page_table
        .translate(mid_text.floor())
        .unwrap()
        .writable());
    assert!(!kernel_space
        .page_table
        .translate(mid_rodata.floor())
        .unwrap()
        .writable());
    assert!(!kernel_space
        .page_table
        .translate(mid_data.floor())
        .unwrap()
        .executable());
    info!("remap_test passed!");
}

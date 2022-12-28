//! Implementation of [`PageTableEntry`] and [`PageTable`].

use super::{frame_alloc, FrameTracker, PhysPageNum, StepByOne, VirtAddr, VirtPageNum};
use alloc::vec;
use alloc::vec::Vec;
use bitflags::*;

// SV39 分页模式下的页表项，[53: 10] 这 44 位是物理页号，最低的 8 位 [7: 0] 是标志位，含义如下：
// 仅当 V(Valid) 位为 1 时，页表项才是合法的；
// R/W/X 分别控制索引到这个页表项的对应虚拟页面是否允许读/写/取指；
// U 控制索引到这个页表项的对应虚拟页面是否在 CPU 处于 U 特权级的情况下是否被允许访问；
// G 我们不理会；
// A(Accessed) 记录自从页表项上的这一位被清零之后，页表项的对应虚拟页面是否被访问过；
// D(Dirty) 则记录自从页表项上的这一位被清零之后，页表项的对应虚拟页表是否被修改过。
bitflags! {
    /// page table entry flags
    pub struct PTEFlags: u8 {
        const V = 1 << 0;
        const R = 1 << 1;
        const W = 1 << 2;
        const X = 1 << 3;
        const U = 1 << 4;
        const G = 1 << 5;
        const A = 1 << 6;
        const D = 1 << 7;
    }
}

#[derive(Copy, Clone)]
#[repr(C)]
/// page table entry structure
/// 页表项结构体实现
pub struct PageTableEntry {
    // bits 即为页表项内容，后十位为状态位
    pub bits: usize,
}

impl PageTableEntry {
    // 物理页号和标志位组合得到页表项内容
    pub fn new(ppn: PhysPageNum, flags: PTEFlags) -> Self {
        PageTableEntry {
            bits: ppn.0 << 10 | flags.bits as usize,
        }
    }

    // 返回一个空的页表项，注意 V 位为 0，它是一个不合法的
    pub fn empty() -> Self {
        PageTableEntry { bits: 0 }
    }

    // 根据页表项得到物理页号 [53: 10] 共 44 位
    pub fn ppn(&self) -> PhysPageNum {
        (self.bits >> 10 & ((1usize << 44) - 1)).into()
    }

    // 根据页表项得到标志位 [7: 0] 共 8 位
    pub fn flags(&self) -> PTEFlags {
        PTEFlags::from_bits(self.bits as u8).unwrap()
    }

    /* 一些辅助函数 */

    // 判断 V 项是否为 1
    pub fn is_valid(&self) -> bool {
        (self.flags() & PTEFlags::V) != PTEFlags::empty()
    }

    // 判断 R 项是否为 1
    pub fn readable(&self) -> bool {
        (self.flags() & PTEFlags::R) != PTEFlags::empty()
    }

    // 判断 W 项是否为 1
    pub fn writable(&self) -> bool {
        (self.flags() & PTEFlags::W) != PTEFlags::empty()
    }

    // 判断 X 项是否为 1
    pub fn executable(&self) -> bool {
        (self.flags() & PTEFlags::X) != PTEFlags::empty()
    }
}

/// 页表结构体
/// SV39 多级页表是以节点为单位进行管理的。每个节点恰好存储在一个物理页帧中，它的位置可以用一个物理页号来表示
/// 当 PageTable 生命周期结束后，向量 frames 里面的那些 FrameTracker 也会被回收，也就意味着存放多级页表节点的那些物理页帧被回收了
pub struct PageTable {
    // 页表起始地址
    root_ppn: PhysPageNum,
    // 页表所有的节点（包括根节点）所在的物理页帧
    frames: Vec<FrameTracker>,
}

/// 页表结构体的一些方法实现
/// Assume that it won't oom when creating/mapping.
impl PageTable {
    pub fn new() -> Self {
        // 分配新的页帧
        let frame = frame_alloc().unwrap();
        PageTable {
            root_ppn: frame.ppn,
            frames: vec![frame],
        }
    }
    /// 临时创建一个专门用来手动查页表的 PageTable，它仅有一个从传入的 satp token 中得到的多级页表根节点的物理页号，它的 frames 字段为空，也即不实际控制任何资源
    /// Temporarily used to get arguments from user space.
    pub fn from_token(satp: usize) -> Self {
        Self {
            root_ppn: PhysPageNum::from(satp & ((1usize << 44) - 1)),
            frames: Vec::new(),
        }
    }

    /// 在多级页表找到一个虚拟页号对应的页表项的可变引用方便后续的读写，如果在遍历的过程中发现有节点尚未创建则会新建一个节点
    fn find_pte_create(&mut self, vpn: VirtPageNum) -> Option<&mut PageTableEntry> {
        // 获取虚拟页号对应的三级页索引（每 9 位为一级页索引）
        let mut idxs = vpn.indexes();
        // 获得当前结点的物理页号，当前为当前结点的物理页号
        let mut ppn = self.root_ppn;

        //
        let mut result: Option<&mut PageTableEntry> = None;

        // 分别取出各级页索引，数组索引为 0 表示一级索引
        // 例如 虚拟地址为 000100001 000100010 000100011，三级页索引为 33, 34, 35，若当前 ppn 为 100，即从物理页号为 100 的页中取出下标为 33 的页表项，然后假设下标为 33 的页表项中存放的第二级页表物理地址（物理页号）为 200，然后再找物理页号为 200 的页表，访问该页表的第 34 个表项，再假设下标为 34 的页表项中存放的第三级页表物理地址（物理页号）为 300，然后再找物理页号为 300 的页表，访问该页表的第 35 个表项。
        // 代码保证返回的 result 是无效的页表项
        // FIXME 若第三级页表项为 invalid，新建一个页表项之后返回的仍然是空？为什么一个 ppn 能有 result 和 pte 两个可变引用？
        for (i, idx) in idxs.iter_mut().enumerate() {
            // 取出各级索引对应的页表项，idx 即为虚拟页号，即上述的 33, 34, 35
            let pte = &mut ppn.get_pte_array()[*idx];
            // 取出第三级索引的页表项
            if i == 2 {
                result = Some(pte);
                break;
            }
            // 如果发现有页表项没有被创建（或无效），则新建一个页表项
            if !pte.is_valid() {
                let frame = frame_alloc().unwrap();
                *pte = PageTableEntry::new(frame.ppn, PTEFlags::V);
                self.frames.push(frame);
            }
            ppn = pte.ppn();
        }
        result
    }

    /// find_pte 和之前的 find_pte_create 不同之处在于它不会试图分配物理页帧。一旦在多级页表上遍历遇到空指针它就会直接返回 None 表示无法正确找到传入的虚拟页号对应的页表项
    pub fn find_pte(&self, vpn: VirtPageNum) -> Option<&PageTableEntry> {
        let idxs = vpn.indexes();
        let mut ppn = self.root_ppn;
        let mut result: Option<&PageTableEntry> = None;
        for (i, idx) in idxs.iter().enumerate() {
            let pte = &ppn.get_pte_array()[*idx];
            if i == 2 {
                result = Some(pte);
                break;
            }
            if !pte.is_valid() {
                return None;
            }
            ppn = pte.ppn();
        }
        result
    }

    /// 操作系统动态维护一个虚拟页号到页表项的映射，支持插入/删除键值对
    #[allow(unused)]

    /// map 方法来在多级页表中插入一个键值对，注意这里我们将物理页号 ppn 和页表项标志位 flags 作为不同的参数传入而不是整合为一个页表项，应该保证 ppn 已经申请
    /// 代码保证 pte 必须是没有被分配的页表项，map 作用是将第三级页表项分配一个物理页，find_pte_create 已经保证分配了前两级页表
    pub fn map(&mut self, vpn: VirtPageNum, ppn: PhysPageNum, flags: PTEFlags) {
        // pte 必须是 invalid，即未被分配
        let pte = self.find_pte_create(vpn).unwrap();
        assert!(!pte.is_valid(), "vpn {:?} is mapped before mapping", vpn);
        *pte = PageTableEntry::new(ppn, flags | PTEFlags::V);
    }

    #[allow(unused)]
    /// 相对的，我们通过 unmap 方法来删除一个键值对，在调用时仅需给出作为索引的虚拟页号即可
    pub fn unmap(&mut self, vpn: VirtPageNum) {
        let pte = self.find_pte_create(vpn).unwrap();
        assert!(pte.is_valid(), "vpn {:?} is invalid before unmapping", vpn);
        *pte = PageTableEntry::empty();
    }

    /// 调用 find_pte 来实现，如果能够找到页表项，那么它会将页表项拷贝一份并返回，否则就 返回一个 None
    pub fn translate(&self, vpn: VirtPageNum) -> Option<PageTableEntry> {
        self.find_pte(vpn).copied()
    }

    /// 地址空间高 256G 是用户空间，低 256G 是内核空间
    /// 
    /// PageTable::token 会按照 satp CSR 格式要求 构造一个无符号 64 位无符号整数，使得其分页模式为 SV39 ，且将当前多级页表的根节点所在的物理页号填充进去
    pub fn token(&self) -> usize {
        8usize << 60 | self.root_ppn.0
    }
}

/// translate a pointer to a mutable u8 Vec through page table
/// 
/// 同样由于内核和应用地址空间的隔离， sys_write 不再能够直接访问位于应用空间中的数据，而需要手动查页表才能知道那些 数据被放置在哪些物理页帧上并进行访问。
/// 
/// 为此，页表模块 page_table 提供了将应用地址空间中一个缓冲区转化为在内核空间中能够直接访问的形式的辅助函数
pub fn translated_byte_buffer(token: usize, ptr: *const u8, len: usize) -> Vec<&'static mut [u8]> {
    let page_table = PageTable::from_token(token);
    let mut start = ptr as usize;
    let end = start + len;
    let mut v = Vec::new();
    while start < end {
        let start_va = VirtAddr::from(start);
        let mut vpn = start_va.floor();
        let ppn = page_table.translate(vpn).unwrap().ppn();
        vpn.step();
        let mut end_va: VirtAddr = vpn.into();
        end_va = end_va.min(VirtAddr::from(end));
        if end_va.page_offset() == 0 {
            v.push(&mut ppn.get_bytes_array()[start_va.page_offset()..]);
        } else {
            v.push(&mut ppn.get_bytes_array()[start_va.page_offset()..end_va.page_offset()]);
        }
        start = end_va.into();
    }
    v
}

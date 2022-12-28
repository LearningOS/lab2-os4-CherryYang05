//! Types related to task management
use super::TaskContext;
use crate::config::{kernel_stack_position, TRAP_CONTEXT, MAX_SYSCALL_NUM};
use crate::mm::{MapPermission, MemorySet, PhysPageNum, VirtAddr, KERNEL_SPACE};
use crate::trap::{trap_handler, TrapContext};

/// task control block structure
pub struct TaskControlBlock {
    pub task_status: TaskStatus,
    pub task_cx: TaskContext,
    // 应用的地址空间
    pub memory_set: MemorySet,
    // 位于应用地址空间次高页的 Trap 上下文被实际存放在物理页帧的物理页号
    pub trap_cx_ppn: PhysPageNum,
    // 统计了应用数据的大小，也就是在应用地址空间中从 0x0 开始到用户栈结束一共包含多少字节
    pub base_size: usize,

    // 记录每个系统调用的次数
    pub syscall_times: [u32; MAX_SYSCALL_NUM],
    // 记录开始时间，便于管理时间片
    pub start_time: usize,
}

impl TaskControlBlock {

    // 获得在用户空间的 Trap 上下文的可变引用用于初始化
    pub fn get_trap_cx(&self) -> &'static mut TrapContext {
        self.trap_cx_ppn.get_mut()
    }

    pub fn get_user_token(&self) -> usize {
        self.memory_set.token()
    }


    pub fn new(elf_data: &[u8], app_id: usize) -> Self {
        // memory_set with elf program headers/trampoline/trap context/user stack
        // 解析传入的 ELF 格式数据构造应用的地址空间 memory_set 并获得其他信息
        let (memory_set, user_sp, entry_point) = MemorySet::from_elf(elf_data);
        // 从地址空间 memory_set 中查多级页表找到应用地址空间中的 Trap 上下文实际被放在哪个物理页帧
        let trap_cx_ppn = memory_set
            .translate(VirtAddr::from(TRAP_CONTEXT).into())
            .unwrap()
            .ppn();
        let task_status = TaskStatus::Ready;

        // map a kernel-stack in kernel space
        // 根据传入的应用 ID app_id 调用在 config 子模块中定义的 kernel_stack_position 找到 应用的内核栈预计放在内核地址空间 KERNEL_SPACE 中的哪个位置，并通过 insert_framed_area 实际将这个逻辑段 加入到内核地址空间中
        let (kernel_stack_bottom, kernel_stack_top) = kernel_stack_position(app_id);
        KERNEL_SPACE.lock().insert_framed_area(
            kernel_stack_bottom.into(),
            kernel_stack_top.into(),
            MapPermission::R | MapPermission::W,
        );

        let task_control_block = Self {
            task_status,
            task_cx: TaskContext::goto_trap_return(kernel_stack_top),
            memory_set,
            trap_cx_ppn,
            base_size: user_sp,
            syscall_times: [0; MAX_SYSCALL_NUM],
            start_time: 0
        };
        // prepare TrapContext in user space
        let trap_cx = task_control_block.get_trap_cx();
        *trap_cx = TrapContext::app_init_context(
            entry_point,
            user_sp,
            KERNEL_SPACE.lock().token(),
            kernel_stack_top,
            trap_handler as usize,
        );
        task_control_block
    }
}

#[derive(Copy, Clone, PartialEq)]
/// task status: UnInit, Ready, Running, Exited
pub enum TaskStatus {
    UnInit,
    Ready,
    Running,
    Exited,
}

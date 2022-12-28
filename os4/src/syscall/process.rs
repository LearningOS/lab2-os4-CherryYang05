//! Process management syscalls

use riscv::register::satp::{self};

use crate::config::{MAX_SYSCALL_NUM, PAGE_SIZE};
use crate::mm::{PageTable, PhysAddr, VirtAddr};
use crate::task::{
    exit_current_and_run_next, get_start_time, get_syscall_times, get_task_status,
    suspend_current_and_run_next, TaskStatus, mmap, munmap, current_user_token,
};
use crate::timer::get_time_us;

#[repr(C)]
#[derive(Debug)]
pub struct TimeVal {
    pub sec: usize,
    pub usec: usize,
}

#[derive(Clone, Copy)]
pub struct TaskInfo {
    pub status: TaskStatus,
    pub syscall_times: [u32; MAX_SYSCALL_NUM],
    pub time: usize,
}

pub fn sys_exit(exit_code: i32) -> ! {
    info!("[kernel] Application exited with code {}", exit_code);
    exit_current_and_run_next();
    panic!("Unreachable in sys_exit!");
}

/// current task gives up resources for other tasks
pub fn sys_yield() -> isize {
    suspend_current_and_run_next();
    0
}

// YOUR JOB: 引入虚地址后重写 sys_get_time
pub fn sys_get_time(_ts: *mut TimeVal, _tz: usize) -> isize {
    let _us = get_time_us();
    let ts = translate_from_virtual_address(_ts as usize) as *mut TimeVal;
    unsafe {
        *ts = TimeVal {
            sec: _us / 1_000_000,
            usec: _us % 1_000_000,
        };
    }
    0
}

// YOUR JOB: 引入虚地址后重写 sys_task_info
pub fn sys_task_info(ti: *mut TaskInfo) -> isize {
    let _ti = translate_from_virtual_address(ti as usize) as *mut TaskInfo;
    unsafe {
        *_ti = TaskInfo {
            status: get_task_status(),
            syscall_times: get_syscall_times(),
            time: (get_time_us() - get_start_time()) / 1000,
        }
    }
    0
}

/// 根据传入的虚拟地址转化为物理地址
pub fn translate_from_virtual_address(vir_addr: usize) -> usize {
    let page_table = PageTable::from_token(current_user_token());
    let virtual_addr = VirtAddr::from(vir_addr);
    let ppn = page_table.find_pte(virtual_addr.floor()).unwrap().ppn();
    PhysAddr::from(ppn).0 + virtual_addr.page_offset()
}

// CLUE: 从 ch4 开始不再对调度算法进行测试~
pub fn sys_set_priority(_prio: isize) -> isize {
    -1
}

// YOUR JOB: 扩展内核以实现 sys_mmap 和 sys_munmap
pub fn sys_mmap(_start: usize, _len: usize, _port: usize) -> isize {
    // _start 要按页对齐
    if _start & (PAGE_SIZE - 1) != 0 {
        return -1;
    }
    
    // _port 其余位必须为 0 且 0-2 位至少有一个为 1
    if _port & 0x7 == 0 || _port & !0x7 != 0 {
        return -1;
    }
    
    mmap(_start, _len, _port)
}

pub fn sys_munmap(_start: usize, _len: usize) -> isize {
    // _start 要按页对齐
    if _start & (PAGE_SIZE - 1) != 0 {
        return -1;
    }
    munmap(_start, _len)
}

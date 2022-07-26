//! Process management syscalls

use crate::config::MAX_SYSCALL_NUM;
use crate::task::{TaskStatus, exit_current_and_run_next, suspend_current_and_run_next};
use crate::timer::get_time_us;
use crate::mm::{MemorySet, translated_physical_address};
use crate::task::current_user_token;
use crate::task::get_task_info;
use crate::task;

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
    let us = get_time_us();
    let ts = translated_physical_address(current_user_token(),
                                         _ts as *const u8) as *mut TimeVal;
    unsafe {
        *ts = TimeVal {
            sec: us / 1_000_000,
            usec: us % 1_000_000,
        };
    }
    0
}

// CLUE: 从 ch4 开始不再对调度算法进行测试~
pub fn sys_set_priority(_prio: isize) -> isize {
    -1
}

// YOUR JOB: 扩展内核以实现 sys_mmap 和 sys_munmap
pub fn sys_mmap(start: usize, len: usize, port: usize) -> isize {
    task::mmap(start, len, port)
}

pub fn sys_munmap(start: usize, len: usize) -> isize {
    task::munmap(start, len)
}

// YOUR JOB: 引入虚地址后重写 sys_task_info
pub fn sys_task_info(ti: *mut TaskInfo) -> isize {
    let ti = translated_physical_address(current_user_token(),
                                         ti as *const u8) as *mut TaskInfo;
    let current_task_info = get_task_info();
    unsafe {
        *ti = TaskInfo {
            status: TaskStatus::Running,
            syscall_times: current_task_info.syscall_times,
            time: (get_time_us() - current_task_info.start_time) / 1000,
        };
    }
    0
}

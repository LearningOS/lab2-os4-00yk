//! Task management implementation
//!
//! Everything about task management, like starting and switching tasks is
//! implemented here.
//!
//! A single global instance of [`TaskManager`] called `TASK_MANAGER` controls
//! all the tasks in the operating system.
//!
//! Be careful when you see [`__switch`]. Control flow around this function
//! might not be what you expect.

mod context;
mod switch;
#[allow(clippy::module_inception)]
mod task;

use crate::config::PAGE_SIZE;
use crate::loader::{get_app_data, get_num_app};
use crate::sync::UPSafeCell;
use crate::trap::TrapContext;
use alloc::vec::Vec;
use lazy_static::*;
pub use switch::__switch;
pub use task::{TaskControlBlock, TaskStatus};

pub use context::TaskContext;

use self::task::TaskStatistics;
use crate::mm;
use crate::mm::{MapPermission, MemorySet, VPNRange, VirtAddr};

/// The task manager, where all the tasks are managed.
///
/// Functions implemented on `TaskManager` deals with all task state transitions
/// and task context switching. For convenience, you can find wrappers around it
/// in the module level.
///
/// Most of `TaskManager` are hidden behind the field `inner`, to defer
/// borrowing checks to runtime. You can see examples on how to use `inner` in
/// existing functions on `TaskManager`.
pub struct TaskManager {
    /// total number of tasks
    num_app: usize,
    /// use inner value to get mutable access
    inner: UPSafeCell<TaskManagerInner>,
}

/// The task manager inner in 'UPSafeCell'
struct TaskManagerInner {
    /// task list
    tasks: Vec<TaskControlBlock>,
    /// id of current `Running` task
    current_task: usize,
}

lazy_static! {
    /// a `TaskManager` instance through lazy_static!
    pub static ref TASK_MANAGER: TaskManager = {
        info!("init TASK_MANAGER");
        let num_app = get_num_app();
        info!("num_app = {}", num_app);
        let mut tasks: Vec<TaskControlBlock> = Vec::new();
        for i in 0..num_app {
            tasks.push(TaskControlBlock::new(get_app_data(i), i));
        }
        TaskManager {
            num_app,
            inner: unsafe {
                UPSafeCell::new(TaskManagerInner {
                    tasks,
                    current_task: 0,
                })
            },
        }
    };
}

impl TaskManager {
    /// Run the first task in task list.
    ///
    /// Generally, the first task in task list is an idle task (we call it zero process later).
    /// But in ch4, we load apps statically, so the first task is a real app.
    fn run_first_task(&self) -> ! {
        let mut inner = self.inner.exclusive_access();
        let next_task = &mut inner.tasks[0];
        next_task.task_status = TaskStatus::Running;
        let next_task_cx_ptr = &next_task.task_cx as *const TaskContext;
        drop(inner);
        let mut _unused = TaskContext::zero_init();
        // before this, we should drop local variables that must be dropped manually
        unsafe {
            __switch(&mut _unused as *mut _, next_task_cx_ptr);
        }
        panic!("unreachable in run_first_task!");
    }

    /// Change the status of current `Running` task into `Ready`.
    fn mark_current_suspended(&self) {
        let mut inner = self.inner.exclusive_access();
        let current = inner.current_task;
        inner.tasks[current].task_status = TaskStatus::Ready;
    }

    /// Change the status of current `Running` task into `Exited`.
    fn mark_current_exited(&self) {
        let mut inner = self.inner.exclusive_access();
        let current = inner.current_task;
        inner.tasks[current].task_status = TaskStatus::Exited;
    }

    /// Find next task to run and return task id.
    ///
    /// In this case, we only return the first `Ready` task in task list.
    fn find_next_task(&self) -> Option<usize> {
        let inner = self.inner.exclusive_access();
        let current = inner.current_task;
        (current + 1..current + self.num_app + 1)
            .map(|id| id % self.num_app)
            .find(|id| inner.tasks[*id].task_status == TaskStatus::Ready)
    }

    /// Get the current 'Running' task's token.
    fn get_current_token(&self) -> usize {
        let inner = self.inner.exclusive_access();
        inner.tasks[inner.current_task].get_user_token()
    }

    #[allow(clippy::mut_from_ref)]
    /// Get the current 'Running' task's trap contexts.
    fn get_current_trap_cx(&self) -> &mut TrapContext {
        let inner = self.inner.exclusive_access();
        inner.tasks[inner.current_task].get_trap_cx()
    }

    /// Switch current `Running` task to the task we have found,
    /// or there is no `Ready` task and we can exit with all applications completed
    fn run_next_task(&self) {
        if let Some(next) = self.find_next_task() {
            let mut inner = self.inner.exclusive_access();
            let current = inner.current_task;
            inner.tasks[next].task_status = TaskStatus::Running;
            inner.current_task = next;
            let current_task_cx_ptr = &mut inner.tasks[current].task_cx as *mut TaskContext;
            let next_task_cx_ptr = &inner.tasks[next].task_cx as *const TaskContext;
            drop(inner);
            // before this, we should drop local variables that must be dropped manually
            unsafe {
                __switch(current_task_cx_ptr, next_task_cx_ptr);
            }
            // go back to user mode
        } else {
            panic!("All applications completed!");
        }
    }
    // Synced from LAB1, bend it to our need
    fn get_task_info(&self) -> TaskStatistics {
        let inner = self.inner.exclusive_access();
        inner.tasks[inner.current_task].task_statistics
    }

    fn update_task_info(&self, syscall_id: usize) {
        let mut inner = self.inner.exclusive_access();
        let cur = inner.current_task;
        inner.tasks[cur].task_statistics.syscall_times[syscall_id] += 1;
    }

    fn mmap(&self, start: usize, len: usize, port: usize) -> isize {
        // sanity check
        // 1. [start, end) start be on page boundaries
        // 2. port must be legal
        if start % PAGE_SIZE != 0 {
            return -1;
        }
        // only R/W/X can be set, R/W/X/ all zero is also not valid
        if port & !0x7 != 0 || port & 0x7 == 0  {
            return -1;
        }
        // according to RISC-V manual, if pte.r = 0 and pte.w = 1, stop and raise an access exception
        if port & 0x2 != 0 && port & 0x1 == 0 {
            return -1;
        }

        let mut inner = self.inner.exclusive_access();
        let current = inner.current_task;
        let ref mut memory_set: MemorySet = inner.tasks[current].memory_set;
        let vpnrange = VPNRange::new(
            VirtAddr::from(start).floor(),
            VirtAddr::from(start + len).ceil(),
        );
        for vpn in vpnrange {
            if let Some(pte) = memory_set.translate(vpn) {
                if pte.is_valid() {
                    // some vpn in range has already been mapped!
                    return -1;
                }
            }
        }
        let mut map_prem = MapPermission::U;
        if (port & 1) != 0 {
            map_prem |= MapPermission::R;
        }
        if (port & 2) != 0 {
            map_prem |= MapPermission::W;
        }
        if (port & 4) != 0 {
            map_prem |= MapPermission::X;
        }
        println!(
            "start_va:{:#x}~end_va:{:#x} map_perm:{:#x}",
            start,
            start + len,
            map_prem
        );
        memory_set.insert_framed_area(VirtAddr::from(start), VirtAddr::from(start + len), map_prem);
        0
    }

    fn munmap(&self, start: usize, len: usize) -> isize {
        // sanity check
        // [start, end) start be on page boundaries
        if start % PAGE_SIZE != 0 {
            return -1;
        }

        let mut inner = self.inner.exclusive_access();
        let current = inner.current_task;
        let ref mut memory_set: MemorySet = inner.tasks[current].memory_set;
        let vpnrange = VPNRange::new(
            VirtAddr::from(start).floor(),
            VirtAddr::from(start + len).ceil(),
        );
        for vpn in vpnrange {
            let pte = memory_set.translate(vpn);
            // 1st-level or 2nd-level pagetable pte invalid || 3rd-level pagetable pte invalid
            if pte.is_none() || !pte.unwrap().is_valid() {
                return -1;
            }
        }
        for vpn in vpnrange {
            memory_set.munmap(vpn);
        }
        0
    }
}

// Synced from LAB1, bend it to our need
pub fn get_task_info() -> TaskStatistics {
    TASK_MANAGER.get_task_info()
}

pub fn update_task_info(syscall_id: usize) {
    TASK_MANAGER.update_task_info(syscall_id)
}

// Added by LAB2
pub fn mmap(start: usize, len: usize, port: usize) -> isize {
    TASK_MANAGER.mmap(start, len, port)
}

pub fn munmap(start: usize, len: usize) -> isize {
    TASK_MANAGER.munmap(start, len)
}

/// Run the first task in task list.
pub fn run_first_task() {
    TASK_MANAGER.run_first_task();
}

/// Switch current `Running` task to the task we have found,
/// or there is no `Ready` task and we can exit with all applications completed
fn run_next_task() {
    TASK_MANAGER.run_next_task();
}

/// Change the status of current `Running` task into `Ready`.
fn mark_current_suspended() {
    TASK_MANAGER.mark_current_suspended();
}

/// Change the status of current `Running` task into `Exited`.
fn mark_current_exited() {
    TASK_MANAGER.mark_current_exited();
}

/// Suspend the current 'Running' task and run the next task in task list.
pub fn suspend_current_and_run_next() {
    mark_current_suspended();
    run_next_task();
}

/// Exit the current 'Running' task and run the next task in task list.
pub fn exit_current_and_run_next() {
    mark_current_exited();
    run_next_task();
}

/// Get the current 'Running' task's token.
pub fn current_user_token() -> usize {
    TASK_MANAGER.get_current_token()
}

/// Get the current 'Running' task's trap contexts.
pub fn current_trap_cx() -> &'static mut TrapContext {
    TASK_MANAGER.get_current_trap_cx()
}

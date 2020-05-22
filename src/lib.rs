// I think there's nothing wrong with this pattern, so silence clippy about it.
#![allow(clippy::try_err)]

mod win_result;
mod winapi_extra;

use crate::win_result::WinError;
use crate::win_result::WinResult;
use crate::winapi_extra::*;

use scopeguard::guard;
use scopeguard::ScopeGuard;

use std::cell::Cell;
use std::collections::VecDeque;
use std::fmt::Debug;

use std::io;
use std::iter::once;
use std::iter::repeat;
use std::mem::align_of;

use std::mem::size_of;
use std::mem::size_of_val;

use std::mem::MaybeUninit;

use std::ptr::null_mut;

use winapi::shared::basetsd::*;
use winapi::shared::minwindef::*;
use winapi::shared::winerror::*;
use winapi::um::handleapi::*;
use winapi::um::processthreadsapi::*;
use winapi::um::synchapi::*;
use winapi::um::winbase::*;
use winapi::um::winnt::*;

trait Job {
  fn on_schedule(
    &self,
    _scheduler_state: &mut SchedulerState,
    _invoker_ums_thread_context: *mut UMS_CONTEXT,
  ) -> Option<Execution> {
    None
  }
  fn main(&self, _thread_state: &mut ThreadState) {}
  fn on_blocking(
    &self,
    _scheduler_state: &mut SchedulerState,
    _cause: ThreadBlockedCause,
  ) -> Option<Execution> {
    None
  }
  fn on_complete(
    &self,
    _scheduler_state: &mut SchedulerState,
  ) -> Option<Execution> {
    None
  }
}

enum TopLevelJobState<F, R>
where
  F: FnOnce() -> R + Send + 'static,
{
  Empty,
  Function(F),
  Result(R),
}
struct TopLevelJob<F, R>
where
  F: FnOnce() -> R + Send + 'static,
{
  state: Cell<TopLevelJobState<F, R>>,
}
impl<F, R> TopLevelJob<F, R>
where
  F: FnOnce() -> R + Send + 'static,
{
  pub fn new(f: F) -> Self {
    let state = TopLevelJobState::Function(f);
    let state = Cell::new(state);
    Self { state }
  }
}
impl<F, R> Job for TopLevelJob<F, R>
where
  F: FnOnce() -> R + Send + 'static,
{
  fn main(&self, _scheduler_state: &mut ThreadState) {
    use TopLevelJobState as S;
    let state = self.state.replace(S::Empty);
    self.state.set(match state {
      S::Function(f) => S::Result(f()),
      _ => unreachable!(),
    })
  }
  fn on_complete(
    &self,
    _scheduler_state: &mut SchedulerState,
  ) -> Option<Execution> {
    eprintln!("Top level job done.");
    None
  }
}

enum BlockingJobState<F, R>
where
  F: FnOnce() -> R + Send + 'static,
{
  Empty,
  Function(F),
  Result(R),
}
struct BlockingJob<F, R>
where
  F: FnOnce() -> R + Send + 'static,
{
  invoker: *mut UMS_CONTEXT,
  event: Cell<ThreadEvent>,
  state: Cell<BlockingJobState<F, R>>,
  counts: Cell<[usize; 2]>,
}
impl<F, R> BlockingJob<F, R>
where
  F: FnOnce() -> R + Send + 'static,
  R: 'static,
{
  pub fn new(f: F) -> Self {
    Self {
      invoker: unsafe { GetCurrentUmsThread() }.unwrap(),
      event: Default::default(),
      state: Cell::new(BlockingJobState::Function(f)),
      counts: Default::default(),
    }
  }
  pub fn run(&self) -> R {
    self.event.set(ThreadEvent::ScheduleJob(self as &dyn Job));
    unsafe { UmsThreadYield(self.event.as_ptr() as *mut VOID) }.unwrap();
    match self.state.replace(BlockingJobState::Empty) {
      BlockingJobState::Result(r) => r,
      _ => unreachable!(),
    }
  }
}
impl<F, R> Job for BlockingJob<F, R>
where
  F: FnOnce() -> R + Send + 'static,
{
  fn main(&self, _scheduler_state: &mut ThreadState) {
    use BlockingJobState as S;
    let state = self.state.replace(S::Empty);
    self.state.set(match state {
      S::Function(f) => S::Result(f()),
      _ => unreachable!(),
    })
  }
  fn on_blocking(
    &self,
    _scheduler_state: &mut SchedulerState,
    cause: ThreadBlockedCause,
  ) -> Option<Execution> {
    let mut counts = self.counts.get();
    counts[cause as usize] += 1;
    self.counts.set(counts);
    None
  }
  fn on_complete(
    &self,
    _scheduler_state: &mut SchedulerState,
  ) -> Option<Execution> {
    println!(
      "Blocking job done. trap# = {}, syscall# = {}",
      self.counts.get()[0],
      self.counts.get()[1]
    );
    Some(Execution::new(
      self.invoker,
      &self.event,
      ThreadEvent::CompleteJob,
    ))
  }
}

pub fn blocking<F, R>(f: F) -> R
where
  F: FnOnce() -> R + Send + 'static,
  R: 'static,
{
  let job = BlockingJob::new(f);
  job.run()
}

#[derive(Copy, Clone)]
enum ThreadEvent {
  None,
  // Thread asks scheduler to delegate a job.
  ScheduleJob(*const dyn Job),
  // Scheduler reports completion of a delegated job.
  CompleteJob,
  // Thread notifies scheduler of being available.
  ThreadStartup,
  // Scheduler notifies thread that it should exit.
  ThreadShutdown,
  // Scheduler assigns job to thread.
  ThreadAssignedJob(*const dyn Job),
  // Thread notifies scheduler of job completion.
  ThreadCompleteJob(*const dyn Job),
}

impl Default for ThreadEvent {
  fn default() -> Self {
    Self::None
  }
}

struct ThreadState {
  event: Cell<ThreadEvent>,
}

impl ThreadState {
  pub fn new() -> Self {
    Self {
      event: Default::default(),
    }
  }

  pub fn set_event(&self, event: ThreadEvent) {
    self.event.set(event);
  }

  pub fn yield_event(&self, event: ThreadEvent) {
    self.set_event(event);
    let param = &self.event as *const Cell<ThreadEvent>
      as *mut Cell<ThreadEvent> as *mut VOID;
    unsafe { UmsThreadYield(param) }.unwrap();
  }

  pub fn get_event(&self) -> ThreadEvent {
    self.event.get()
  }
}

unsafe extern "system" fn thread_main(_: *mut VOID) -> DWORD {
  let mut state = ThreadState::new();
  state.yield_event(ThreadEvent::ThreadStartup);
  loop {
    match state.get_event() {
      ThreadEvent::ThreadAssignedJob(job) => {
        (*job).main(&mut state);
        state.yield_event(ThreadEvent::ThreadCompleteJob(job))
      }
      ThreadEvent::ThreadShutdown => {
        while current_thread_has_io_pending().unwrap() {
          match SleepEx(10_000, TRUE) {
            0 => break,
            WAIT_IO_COMPLETION => {}
            _ => unreachable!(),
          }
        }
        println!("Thread stopped");
        return 0;
      }
      _ => unreachable!(),
    }
  }
}

fn current_thread_has_io_pending() -> Result<bool, io::Error> {
  unsafe {
    let mut io_pending_flag = FALSE;
    GetThreadIOPendingFlag(GetCurrentThread(), &mut io_pending_flag)
      .result()
      .map(|_| match io_pending_flag {
        TRUE => true,
        FALSE => false,
        _ => unreachable!(),
      })
  }
}

struct Executor {
  ums_thread_context: *mut UMS_CONTEXT,
  event_slot: *const Cell<ThreadEvent>,
}

impl Executor {
  pub fn new(
    ums_thread_context: *mut UMS_CONTEXT,
    event_slot: *const Cell<ThreadEvent>,
  ) -> Self {
    Self {
      ums_thread_context,
      event_slot,
    }
  }
}

struct Execution {
  ums_thread_context: *mut UMS_CONTEXT,
  event_slot: *const Cell<ThreadEvent>,
  event: ThreadEvent,
}

impl Execution {
  pub fn new(
    ums_thread_context: *mut UMS_CONTEXT,
    event_slot: *const Cell<ThreadEvent>,
    event: ThreadEvent,
  ) -> Self {
    Self {
      ums_thread_context,
      event_slot,
      event,
    }
  }

  pub fn execute(
    self,
    scheduler_state: &mut SchedulerState,
  ) -> Option<*mut UMS_CONTEXT> {
    let ums_thread_context = self.ums_thread_context;
    unsafe { &*self.event_slot }.set(self.event);
    scheduler_state.last_execution = Some(self);
    Some(ums_thread_context)
  }
}

struct SchedulerState {
  pub queued_jobs: VecDeque<*const dyn Job>,
  pub total_job_count: usize,
  pub idle_threads: VecDeque<Executor>,
  pub runnable_threads: VecDeque<*mut UMS_CONTEXT>,
  pub thread_count_starting: usize,
  pub thread_count_total: usize,
  pub last_execution: Option<Execution>,
  pub ums_completion_list: *mut UMS_COMPLETION_LIST,
}

unsafe impl Send for SchedulerState {}
unsafe impl Sync for SchedulerState {}

impl SchedulerState {
  pub fn new(
    ums_completion_list: *mut UMS_COMPLETION_LIST,
    queued_jobs: VecDeque<*const dyn Job>,
  ) -> Self {
    let total_job_count = queued_jobs.len();
    Self {
      queued_jobs,
      total_job_count,
      idle_threads: VecDeque::new(),
      runnable_threads: VecDeque::new(),
      thread_count_starting: 0,
      thread_count_total: 0,
      last_execution: None,
      ums_completion_list,
    }
  }
}

#[derive(Debug)]
enum ThreadBlockedCause {
  Trap,
  SysCall,
}

#[derive(Debug)]
enum ThreadReadyCause {
  Yield { param: *mut VOID },
  Busy,
  Suspended,
  Terminated,
  ExecuteFailed { error: io::Error },
}

#[derive(Debug)]
#[allow(dead_code)] // False positive.
enum SchedulerEntryCause {
  Startup {
    param: *mut VOID,
  },
  ThreadBlocked {
    cause: ThreadBlockedCause,
  },
  ThreadReady {
    ums_thread_context: *mut UMS_CONTEXT,
    cause: ThreadReadyCause,
  },
}

impl SchedulerEntryCause {
  pub fn from_scheduler_entry_args(
    reason: UMS_SCHEDULER_REASON,
    activation_payload: ULONG_PTR,
    scheduler_param: *mut VOID,
  ) -> Self {
    #[allow(non_upper_case_globals)]
    match reason {
      UmsSchedulerStartup => Self::Startup {
        param: scheduler_param,
      },
      UmsSchedulerThreadBlocked if activation_payload == 0 => {
        Self::ThreadBlocked {
          cause: ThreadBlockedCause::Trap,
        }
      }
      UmsSchedulerThreadBlocked if activation_payload == 1 => {
        Self::ThreadBlocked {
          cause: ThreadBlockedCause::SysCall,
        }
      }
      UmsSchedulerThreadYield => Self::ThreadReady {
        cause: ThreadReadyCause::Yield {
          param: scheduler_param,
        },
        ums_thread_context: activation_payload as *mut UMS_CONTEXT,
      },
      _ => unreachable!(),
    }
  }

  pub fn from_execute_error(
    ums_thread_context: *mut UMS_CONTEXT,
    error: DWORD,
  ) -> Self {
    let cause = match error {
      ERROR_RETRY => ThreadReadyCause::Busy,
      ERROR_SUCCESS => unreachable!(),
      _ if Self::get_ums_thread_flag(
        ums_thread_context,
        UmsThreadIsTerminated,
      ) =>
      {
        ThreadReadyCause::Terminated
      }
      _ if Self::get_ums_thread_flag(
        ums_thread_context,
        UmsThreadIsSuspended,
      ) =>
      {
        ThreadReadyCause::Suspended
      }
      error => {
        let error = io::Error::from_raw_os_error(error as i32);
        ThreadReadyCause::ExecuteFailed { error }
      }
    };
    Self::ThreadReady {
      ums_thread_context,
      cause,
    }
  }

  fn get_ums_thread_flag(
    ums_thread_context: *mut UMS_CONTEXT,
    ums_thread_info_class: UMS_THREAD_INFO_CLASS,
  ) -> bool {
    get_ums_thread_info::<BOOLEAN, ()>(
      ums_thread_context,
      ums_thread_info_class,
    )
    .map(|flag| flag != 0)
    .unwrap_or(false)
  }
}

extern "system" fn scheduler_entry(
  reason: UMS_SCHEDULER_REASON,
  activation_payload: ULONG_PTR,
  scheduler_param: *mut VOID,
) {
  // The `ExecuteUmsThread()` does not return if succesful, and certainly Rust
  // does not expect that, so we are at risk of skipping Drop handlers.
  // Therefore, most of the actual work is done in `scheduler_entry_impl()`.
  // We just have to be sure that no variables in this function require cleanup.
  let mut cause = SchedulerEntryCause::from_scheduler_entry_args(
    reason,
    activation_payload,
    scheduler_param,
  );
  while let Some(ums_thread_context) = scheduler_entry_impl(cause) {
    // `ExecuteUmsThread()` does not return unless it fails.
    let error = unsafe { ExecuteUmsThread(ums_thread_context) }.unwrap_err();
    cause = SchedulerEntryCause::from_execute_error(ums_thread_context, error);
  }
}

fn scheduler_entry_impl(
  cause: SchedulerEntryCause,
) -> Option<*mut UMS_CONTEXT> {
  use SchedulerEntryCause::*;

  eprintln!("S: {:?}", cause);

  let state = unsafe {
    GetCurrentUmsThread()
      .result()
      .and_then(|ums_thread_context| match cause {
        Startup { param } => {
          let state = param as *mut SchedulerState;
          set_ums_thread_info(ums_thread_context, UmsThreadUserContext, state)
            .map(|_| state)
        }
        _ => get_ums_thread_info(ums_thread_context, UmsThreadUserContext),
      })
      .map(|state| &mut *state)
      .unwrap()
  };

  let next_execution = match cause {
    Startup { .. } => None,
    ThreadBlocked { cause } => state
      .last_execution
      .as_ref()
      .and_then(|e| match e.event {
        ThreadEvent::ThreadAssignedJob(job) => Some(job),
        _ => None,
      })
      .and_then(|job| unsafe { (*job).on_blocking(state, cause) }),
    ThreadReady {
      ums_thread_context,
      cause: ThreadReadyCause::Yield { param },
    } => {
      let event_slot = unsafe { &*(param as *mut Cell<ThreadEvent>) };
      let event = event_slot.get();
      match event {
        ThreadEvent::ThreadStartup => {
          state
            .idle_threads
            .push_back(Executor::new(ums_thread_context, event_slot));
          state.thread_count_starting -= 1;
          None
        }
        ThreadEvent::ScheduleJob(job) => {
          state.queued_jobs.push_back(job);
          state.total_job_count += 1;
          unsafe { (*job).on_schedule(state, ums_thread_context) }
        }
        ThreadEvent::ThreadCompleteJob(job) => {
          state
            .idle_threads
            .push_back(Executor::new(ums_thread_context, event_slot));
          state.total_job_count -= 1;
          unsafe { (*job).on_complete(state) }
        }
        _ => unreachable!(),
      }
    }
    ThreadReady {
      ums_thread_context,
      cause: ThreadReadyCause::Terminated,
    } => {
      unsafe { DeleteUmsThreadContext(ums_thread_context) }.unwrap();
      state.thread_count_total -= 1;
      None
    }
    ThreadReady {
      ums_thread_context,
      cause: ThreadReadyCause::ExecuteFailed { error },
    } => panic!("ExecuteUmsThread({:?}): {}", ums_thread_context, error),
    ThreadReady {
      ums_thread_context, ..
    } => {
      state.runnable_threads.push_back(ums_thread_context);
      None
    }
  };
  state.last_execution.take();

  if let Some(next_execution) = next_execution {
    return next_execution.execute(state);
  }
  if !state.queued_jobs.is_empty() && !state.idle_threads.is_empty() {
    let job = state.queued_jobs.pop_front().unwrap();
    let executor = state.idle_threads.pop_back().unwrap();
    let execution = Execution::new(
      executor.ums_thread_context,
      executor.event_slot,
      ThreadEvent::ThreadAssignedJob(job),
    );
    return execution.execute(state);
  }
  if state.total_job_count > 0 {
    // Create extra UMS threads. We aim to always have one spare.
    // If there are no jobs at all this step is skipped, as we should be shutting
    // down rather than bringing new threads up.
    const THREADS_SPARE: usize = 1;
    let threads_avail = state.idle_threads.len() + state.thread_count_starting;
    let threads_needed = state.queued_jobs.len() + THREADS_SPARE;
    let threads_short = threads_needed.saturating_sub(threads_avail);
    for _ in 0..threads_short {
      create_ums_thread(state.ums_completion_list).unwrap();
      state.thread_count_starting += 1;
      state.thread_count_total += 1;
    }
  } else {
    // No more jobs, so shut down threads.
    if let Some(executor) = state.idle_threads.pop_back() {
      let execution = Execution::new(
        executor.ums_thread_context,
        executor.event_slot,
        ThreadEvent::ThreadShutdown,
      );
      return execution.execute(state);
    }
    if state.thread_count_total == 0 {
      assert!(state.thread_count_starting == 0);
      return None;
    }
  }
  eprintln!(
    "#### {:?} {:?} {:?}",
    state.idle_threads.len(),
    state.thread_count_total,
    state.total_job_count,
  );

  loop {
    eprintln!("selecting. ready# = {}", state.runnable_threads.len());
    match state.runnable_threads.pop_front() {
      Some(ums_thread_context) => {
        eprintln!("scheduling: {:X?}", ums_thread_context);
        break Some(ums_thread_context);
      }
      None => {
        eprintln!("waiting for ready threads");
        let mut ums_thread_context_list_head: *mut UMS_CONTEXT = null_mut();
        unsafe {
          DequeueUmsCompletionListItems(
            state.ums_completion_list,
            INFINITE,
            &mut ums_thread_context_list_head,
          )
        }
        .unwrap();
        while !(ums_thread_context_list_head.is_null()) {
          eprintln!("w<r");
          state
            .runnable_threads
            .push_back(ums_thread_context_list_head);
          ums_thread_context_list_head =
            unsafe { GetNextUmsListItem(ums_thread_context_list_head) };
        }
      }
    }
  }
}

pub fn run_ums_scheduler<F, R>(f: F) -> io::Result<()>
where
  F: FnOnce() -> R + Send + 'static,
  R: 'static,
{
  unsafe {
    let mut ums_completion_list: *mut UMS_COMPLETION_LIST = null_mut();
    CreateUmsCompletionList(&mut ums_completion_list).result()?;
    assert!(!ums_completion_list.is_null());
    let ums_completion_list = guard(ums_completion_list, |l| {
      DeleteUmsCompletionList(l).unwrap();
    });

    let top_level_job = TopLevelJob::new(f);
    let queued_jobs = once(&top_level_job)
      .map(|job| job as &dyn Job)
      .map(|job| job as *const dyn Job)
      .collect::<VecDeque<_>>();
    let mut scheduler_state =
      SchedulerState::new(*ums_completion_list, queued_jobs);

    let mut scheduler_startup_info = UMS_SCHEDULER_STARTUP_INFO {
      UmsVersion: UMS_VERSION,
      CompletionList: *ums_completion_list,
      SchedulerProc: Some(scheduler_entry),
      SchedulerParam: &mut scheduler_state as *mut _ as *mut VOID,
    };
    EnterUmsSchedulingMode(&mut scheduler_startup_info).result()
  }
}

fn create_ums_thread(
  ums_completion_list: *mut UMS_COMPLETION_LIST,
) -> io::Result<*mut UMS_CONTEXT> {
  unsafe {
    let mut ums_thread_context = null_mut();
    CreateUmsThreadContext(&mut ums_thread_context).result()?;
    let ums_thread_context = guard(ums_thread_context, |c| {
      DeleteUmsThreadContext(c).unwrap();
    });

    let mut attr_list_size = 0;
    InitializeProcThreadAttributeList(null_mut(), 1, 0, &mut attr_list_size)
      .unwrap_err::<()>();
    assert_ne!(attr_list_size, 0);

    let mut attr_list = repeat(0u8).take(attr_list_size).collect::<Box<[u8]>>();
    let attr_list: *mut PROC_THREAD_ATTRIBUTE_LIST =
      cast_aligned(attr_list.as_mut_ptr());

    InitializeProcThreadAttributeList(attr_list, 1, 0, &mut attr_list_size)
      .result()?;
    let attr_list = guard(attr_list, |p| DeleteProcThreadAttributeList(p));

    let attr = UMS_CREATE_THREAD_ATTRIBUTES {
      UmsVersion: UMS_VERSION,
      UmsContext: *ums_thread_context,
      UmsCompletionList: ums_completion_list,
    };

    UpdateProcThreadAttribute(
      *attr_list,
      0,
      PROC_THREAD_ATTRIBUTE_UMS_THREAD,
      cast_mut_void(&attr),
      size_of_val(&attr),
      null_mut(),
      null_mut(),
    )
    .result()?;

    let thread_handle = CreateRemoteThreadEx(
      GetCurrentProcess(),
      null_mut(),
      1 << 20,
      Some(thread_main),
      null_mut(),
      STACK_SIZE_PARAM_IS_A_RESERVATION,
      *attr_list,
      null_mut(),
    )
    .result()?;
    CloseHandle(thread_handle).result()?;

    Ok(ScopeGuard::into_inner(ums_thread_context))
  }
}

fn get_ums_thread_info<T: Copy + 'static, E: WinError>(
  ums_thread_context: *mut UMS_CONTEXT,
  ums_thread_info_class: UMS_THREAD_INFO_CLASS,
) -> Result<T, E> {
  let mut info = MaybeUninit::<T>::uninit();
  let info_size_in = size_of::<T>() as ULONG;
  let mut info_size_out = MaybeUninit::<ULONG>::uninit();
  unsafe {
    QueryUmsThreadInformation(
      ums_thread_context,
      ums_thread_info_class,
      info.as_mut_ptr() as *mut VOID,
      info_size_in,
      info_size_out.as_mut_ptr(),
    )
    .ok_or_else(E::get_last_error)
    .map(move |_| assert_eq!(info_size_in, info_size_out.assume_init()))
    .map(move |_| info.assume_init())
  }
}

fn set_ums_thread_info<T: Copy + 'static, E: WinError>(
  ums_thread_context: *mut UMS_CONTEXT,
  ums_thread_info_class: UMS_THREAD_INFO_CLASS,
  info: T,
) -> Result<(), E> {
  unsafe {
    SetUmsThreadInformation(
      ums_thread_context,
      ums_thread_info_class,
      &info as *const T as *mut T as *mut VOID,
      size_of::<T>() as ULONG,
    )
    .ok_or_else(E::get_last_error)
  }
}

fn cast_aligned<T, U>(p: *mut T) -> *mut U {
  let address = p as usize;
  let mask = align_of::<U>() - 1;
  assert_eq!(address & mask, 0);
  address as *mut U
}

#[allow(dead_code)]
fn cast_mut_void<T>(p: *const T) -> *mut VOID {
  p as *mut T as *mut VOID
}

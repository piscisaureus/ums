use ums::*;
use winapi::um::synchapi::Sleep;
use winapi::um::winbase::INFINITE;

fn main() {
  run_ums_scheduler().unwrap();
  unsafe { Sleep(INFINITE) };
}

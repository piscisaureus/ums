use ums::*;
use winapi::um::synchapi::Sleep;
//use winapi::um::winbase::INFINITE;

fn main() {
  run_ums_scheduler(|| {
    for i in 0..10 {
      eprintln!("I'm top level hehe {}", i);
      unsafe { Sleep(1000) };
    }
  })
  .unwrap();
}

use ums::*;
use winapi::um::synchapi::Sleep;

fn main() {
  run_ums_scheduler(|| {
    for i in 0..4 {
      println!("I'm top level hehe {}", i);
      unsafe { Sleep(3) };
      let isq = blocking(move || {
        println!("  B>");
        unsafe {
          for _ in 0..4 {
            Sleep(500)
          }
        }
        println!("  <B");
        i * i
      });
      println!("i^2={}", isq);
    }
  })
  .unwrap();
}

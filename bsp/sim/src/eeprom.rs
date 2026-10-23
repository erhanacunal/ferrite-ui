//! Virtual 1 KB EEPROM for the simulator (same size as the GD32 Nextion's
//! 24C08). Erased bytes read 0xFF. With `--eeprom <file>` the contents are
//! loaded at start and saved after every write, so values survive restarts
//! like on the device; without it the EEPROM lives in RAM only.

use std::cell::RefCell;
use std::fs;

use ferrite_core::eeprom::EepromBackend;

const SIZE: usize = 1024;

pub struct SimEeprom {
    mem: RefCell<Vec<u8>>,
    path: Option<String>,
}

impl SimEeprom {
    pub fn new(path: Option<String>) -> Self {
        let mut mem = vec![0xFF; SIZE];
        if let Some(p) = &path {
            match fs::read(p) {
                Ok(bytes) => {
                    let n = bytes.len().min(SIZE);
                    mem[..n].copy_from_slice(&bytes[..n]);
                    println!("eeprom: loaded {n} bytes from {p}");
                }
                Err(_) => println!("eeprom: {p} not found, starting erased"),
            }
        }
        Self { mem: RefCell::new(mem), path }
    }
}

impl EepromBackend for SimEeprom {
    const SIZE: u16 = SIZE as u16;

    fn read(&self, addr: u16, buf: &mut [u8]) -> bool {
        let a = addr as usize;
        buf.copy_from_slice(&self.mem.borrow()[a..a + buf.len()]);
        true
    }

    fn write(&self, addr: u16, data: &[u8]) -> bool {
        let a = addr as usize;
        self.mem.borrow_mut()[a..a + data.len()].copy_from_slice(data);
        println!("eeprom: wrote {} byte(s) at {addr}", data.len());
        if let Some(p) = &self.path {
            if let Err(e) = fs::write(p, &*self.mem.borrow()) {
                eprintln!("eeprom: saving {p} failed: {e}");
            }
        }
        true
    }
}

//! External memories as part of the design.
//!
//! A design whose ports address an external memory (a cartridge ROM, a
//! firmware flash, an SRAM chip) is simulated fastest when that memory runs
//! inside the program: [`attach_memory`] adds a memory to the module whose
//! asynchronous read port drives the data input from the address output,
//! with an optional write port. Its contents are then loaded and read back
//! through [`crate::Sim::load_memory`] and [`crate::Sim::read_memory`].

use synth_rtl::{Clock, MemRead, MemWrite, Memory, Module, Sig, SigBit};

/// The write port of an attached memory: on the clock edge, when the
/// enable is high, `data` is written at the read address.
#[derive(Clone, Debug)]
pub struct WritePort<'a> {
    pub clock: &'a str,
    pub posedge: bool,
    pub enable: &'a str,
    pub data: &'a str,
    /// The write address, when it differs from the read address.
    pub addr: Option<&'a str>,
}

fn wire(m: &Module, name: &str) -> Result<Sig, String> {
    m.wires
        .iter_enumerated()
        .find(|(_, w)| w.name == name)
        .map(|(id, w)| Sig::wire(id, w.width))
        .ok_or_else(|| format!("no wire `{name}`"))
}

/// Drives the input port `data` with the word at `addr` of a new memory
/// `name` of `depth` words (initially unknown), which is written through
/// `write` if given. `data` stops being a port.
pub fn attach_memory(
    m: &mut Module,
    name: &str,
    addr: &str,
    data: &str,
    depth: u32,
    write: Option<WritePort>,
) -> Result<(), String> {
    let addr_sig = wire(m, addr)?;
    let data_sig = wire(m, data)?;
    let width = data_sig.width();
    let data_id =
        m.wires.iter_enumerated().find(|(_, w)| w.name == data).map(|(id, _)| id).unwrap();
    m.wires[data_id].port = None;
    let mut writes = Vec::new();
    if let Some(wp) = write {
        let clk = wire(m, wp.clock)?;
        let en = wire(m, wp.enable)?;
        let wdata = wire(m, wp.data)?;
        let waddr = match wp.addr {
            Some(a) => wire(m, a)?,
            None => addr_sig.clone(),
        };
        if wdata.width() != width || clk.width() != 1 || en.width() != 1 {
            return Err(format!("memory `{name}`: write port widths do not match"));
        }
        writes.push(MemWrite {
            clk: Clock { sig: clk.0[0], posedge: wp.posedge },
            en: Sig(vec![en.0[0]; width]),
            addr: waddr,
            data: wdata,
        });
    }
    m.memories.push(Memory {
        name: name.to_string(),
        width: width as u32,
        depth,
        offset: 0,
        init: Vec::new(),
        reads: vec![MemRead {
            clk: None,
            en: SigBit::ONE,
            addr: addr_sig,
            data: data_sig,
            transparent: vec![false; writes.len()],
        }],
        writes,
        alive: true,
    });
    Ok(())
}

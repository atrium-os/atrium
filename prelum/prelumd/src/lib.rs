//! prelumd (printing.md §6.4): [`daemon`] holds the printers and jobs,
//! [`server`] answers `CLASS_PRINT`, [`launch`] starts workers (through
//! jaild in Atrium: [`jail`]), [`store`] keeps saved printers, [`nearby`]
//! finds printers on the LAN.

pub mod daemon;
#[cfg(target_os = "freebsd")]
pub mod jail;
pub mod launch;
pub mod nearby;
pub mod server;
pub mod sheet;
pub mod store;

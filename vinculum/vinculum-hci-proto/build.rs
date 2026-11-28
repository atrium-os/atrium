#[path = "../pdl/generate.rs"]
mod pdl;

fn main() {
    pdl::generate("hci_packets");
    pdl::generate("gap");
}

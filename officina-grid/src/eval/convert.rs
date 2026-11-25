//! CONVERT (docs/spec/officina.md §16.139): a number from one unit to
//! another of the same kind, with Excel's unit names, metric prefixes on the
//! units that take them and binary prefixes on bit and byte. The factors
//! are the units' definitions in SI (or, for information, in bits).

use super::{to_text, Ctx, Ev};
use crate::formula::Expr;
use crate::value::{ErrorKind, Value};

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Mass,
    Length,
    Time,
    Pressure,
    Force,
    Energy,
    Power,
    Magnetism,
    Temperature,
    Volume,
    Area,
    Information,
    Speed,
}

/// A unit: its names, kind, size in the kind's base unit, whether metric
/// prefixes apply, and the power a prefix is raised to (2 for areas, 3 for
/// volumes).
struct Unit {
    names: &'static [&'static str],
    kind: Kind,
    size: f64,
    prefix: bool,
    power: i32,
}

const IN: f64 = 0.0254;
const FT: f64 = 0.3048;
const YD: f64 = 0.9144;
const MI: f64 = 1609.344;
const NMI: f64 = 1852.0;
const LY: f64 = 9_460_730_472_580_800.0;
const PICA_PT: f64 = IN / 72.0;
const LBF: f64 = 4.448_221_615_260_5;
const GAL: f64 = 3.785_411_784e-3;
const HP: f64 = 745.699_871_582_270_2;

macro_rules! u {
    ($kind:ident, $size:expr, $prefix:expr, $power:expr, [$($n:literal),+]) => {
        Unit { names: &[$($n),+], kind: Kind::$kind, size: $size, prefix: $prefix, power: $power }
    };
}

static UNITS: &[Unit] = &[
    // Mass (grams).
    u!(Mass, 1.0, true, 1, ["g"]),
    u!(Mass, LBF / FT * 1000.0, false, 1, ["sg"]),
    u!(Mass, 453.592_37, false, 1, ["lbm"]),
    u!(Mass, 1.660_539_066_60e-24, true, 1, ["u"]),
    u!(Mass, 28.349_523_125, false, 1, ["ozm"]),
    u!(Mass, 0.064_798_91, false, 1, ["grain"]),
    u!(Mass, 45_359.237, false, 1, ["cwt", "shweight"]),
    u!(Mass, 50_802.345_44, false, 1, ["uk_cwt", "lcwt", "hweight"]),
    u!(Mass, 6_350.293_18, false, 1, ["stone"]),
    u!(Mass, 907_184.74, false, 1, ["ton"]),
    u!(Mass, 1_016_046.908_8, false, 1, ["uk_ton", "LTON", "brton"]),
    // Length (metres).
    u!(Length, 1.0, true, 1, ["m"]),
    u!(Length, MI, false, 1, ["mi"]),
    u!(Length, NMI, false, 1, ["Nmi"]),
    u!(Length, IN, false, 1, ["in"]),
    u!(Length, FT, false, 1, ["ft"]),
    u!(Length, YD, false, 1, ["yd"]),
    u!(Length, 1e-10, true, 1, ["ang"]),
    u!(Length, 1.143, false, 1, ["ell"]),
    u!(Length, LY, true, 1, ["ly"]),
    u!(Length, 3.085_677_581_491_367e16, true, 1, ["parsec", "pc"]),
    u!(Length, PICA_PT, false, 1, ["Picapt", "Pica"]),
    u!(Length, IN / 6.0, false, 1, ["pica"]),
    u!(Length, 6336.0 / 3937.0 * 1000.0, false, 1, ["survey_mi"]),
    // Time (seconds).
    u!(Time, 365.25 * 86_400.0, false, 1, ["yr"]),
    u!(Time, 86_400.0, false, 1, ["day", "d"]),
    u!(Time, 3_600.0, false, 1, ["hr"]),
    u!(Time, 60.0, false, 1, ["mn", "min"]),
    u!(Time, 1.0, true, 1, ["sec", "s"]),
    // Pressure (pascals).
    u!(Pressure, 1.0, true, 1, ["Pa", "p"]),
    u!(Pressure, 101_325.0, true, 1, ["atm", "at"]),
    u!(Pressure, 133.322_387_415, true, 1, ["mmHg"]),
    u!(Pressure, LBF / (IN * IN), false, 1, ["psi"]),
    u!(Pressure, 101_325.0 / 760.0, false, 1, ["Torr"]),
    // Force (newtons).
    u!(Force, 1.0, true, 1, ["N"]),
    u!(Force, 1e-5, true, 1, ["dyn", "dy"]),
    u!(Force, LBF, false, 1, ["lbf"]),
    u!(Force, 0.009_806_65, true, 1, ["pond"]),
    // Energy (joules).
    u!(Energy, 1.0, true, 1, ["J"]),
    u!(Energy, 1e-7, true, 1, ["e"]),
    u!(Energy, 4.184, true, 1, ["c"]),
    u!(Energy, 4.1868, true, 1, ["cal"]),
    u!(Energy, 1.602_176_634e-19, true, 1, ["eV", "ev"]),
    u!(Energy, HP * 3_600.0, false, 1, ["HPh", "hh"]),
    u!(Energy, 3_600.0, true, 1, ["Wh", "wh"]),
    u!(Energy, LBF * FT, false, 1, ["flb"]),
    u!(Energy, 1_055.055_852_62, false, 1, ["BTU", "btu"]),
    // Power (watts).
    u!(Power, HP, false, 1, ["HP", "h"]),
    u!(Power, 735.498_75, false, 1, ["PS"]),
    u!(Power, 1.0, true, 1, ["W", "w"]),
    // Magnetism (teslas).
    u!(Magnetism, 1.0, true, 1, ["T"]),
    u!(Magnetism, 1e-4, true, 1, ["ga"]),
    // Temperature (kelvins; offsets in `to_kelvin`).
    u!(Temperature, 1.0, false, 1, ["C", "cel"]),
    u!(Temperature, 1.0, false, 1, ["F", "fah"]),
    u!(Temperature, 1.0, true, 1, ["K", "kel"]),
    u!(Temperature, 1.0, false, 1, ["Rank"]),
    u!(Temperature, 1.0, false, 1, ["Reau"]),
    // Volume (cubic metres).
    u!(Volume, GAL / 768.0, false, 1, ["tsp"]),
    u!(Volume, 5e-6, false, 1, ["tspm"]),
    u!(Volume, GAL / 256.0, false, 1, ["tbs"]),
    u!(Volume, GAL / 128.0, false, 1, ["oz"]),
    u!(Volume, GAL / 16.0, false, 1, ["cup"]),
    u!(Volume, GAL / 8.0, false, 1, ["pt", "us_pt"]),
    u!(Volume, 4.546_09e-3 / 8.0, false, 1, ["uk_pt"]),
    u!(Volume, GAL / 4.0, false, 1, ["qt"]),
    u!(Volume, 4.546_09e-3 / 4.0, false, 1, ["uk_qt"]),
    u!(Volume, GAL, false, 1, ["gal"]),
    u!(Volume, 4.546_09e-3, false, 1, ["uk_gal"]),
    u!(Volume, 1e-3, true, 1, ["l", "L", "lt"]),
    u!(Volume, 1e-30, true, 3, ["ang3", "ang^3"]),
    u!(Volume, 42.0 * GAL, false, 1, ["barrel"]),
    u!(Volume, 35.239_070_166_88e-3, false, 1, ["bushel"]),
    u!(Volume, FT * FT * FT, false, 1, ["ft3", "ft^3"]),
    u!(Volume, IN * IN * IN, false, 1, ["in3", "in^3"]),
    u!(Volume, LY * LY * LY, true, 3, ["ly3", "ly^3"]),
    u!(Volume, 1.0, true, 3, ["m3", "m^3"]),
    u!(Volume, MI * MI * MI, false, 1, ["mi3", "mi^3"]),
    u!(Volume, YD * YD * YD, false, 1, ["yd3", "yd^3"]),
    u!(Volume, NMI * NMI * NMI, false, 1, ["Nmi3", "Nmi^3"]),
    u!(Volume, PICA_PT * PICA_PT * PICA_PT, false, 1, ["Picapt3", "Picapt^3", "Pica3", "Pica^3"]),
    u!(Volume, 100.0 * FT * FT * FT, false, 1, ["GRT", "regton"]),
    u!(Volume, 40.0 * FT * FT * FT, false, 1, ["MTON"]),
    // Area (square metres).
    u!(Area, 4_046.856_422_4, false, 1, ["uk_acre"]),
    u!(Area, 4_046.872_609_874_252, false, 1, ["us_acre"]),
    u!(Area, 1e-20, true, 2, ["ang2", "ang^2"]),
    u!(Area, 100.0, true, 1, ["ar"]),
    u!(Area, FT * FT, false, 1, ["ft2", "ft^2"]),
    u!(Area, 10_000.0, false, 1, ["ha"]),
    u!(Area, IN * IN, false, 1, ["in2", "in^2"]),
    u!(Area, LY * LY, true, 2, ["ly2", "ly^2"]),
    u!(Area, 1.0, true, 2, ["m2", "m^2"]),
    u!(Area, 2_500.0, false, 1, ["Morgen"]),
    u!(Area, MI * MI, false, 1, ["mi2", "mi^2"]),
    u!(Area, NMI * NMI, false, 1, ["Nmi2", "Nmi^2"]),
    u!(Area, PICA_PT * PICA_PT, false, 1, ["Picapt2", "Pica2", "Pica^2", "Picapt^2"]),
    u!(Area, YD * YD, false, 1, ["yd2", "yd^2"]),
    // Information (bits).
    u!(Information, 1.0, true, 1, ["bit"]),
    u!(Information, 8.0, true, 1, ["byte"]),
    // Speed (metres per second).
    u!(Speed, 6_080.0 * FT / 3_600.0, false, 1, ["admkn"]),
    u!(Speed, NMI / 3_600.0, false, 1, ["kn"]),
    u!(Speed, 1.0 / 3_600.0, true, 1, ["m/h", "m/hr"]),
    u!(Speed, MI / 3_600.0, false, 1, ["mph"]),
    u!(Speed, 1.0, true, 1, ["m/s", "m/sec"]),
];

const PREFIXES: &[(&str, f64)] = &[
    ("Y", 1e24), ("Z", 1e21), ("E", 1e18), ("P", 1e15), ("T", 1e12), ("G", 1e9), ("M", 1e6), ("k", 1e3), ("h", 1e2),
    ("da", 1e1), ("e", 1e1), ("d", 1e-1), ("c", 1e-2), ("m", 1e-3), ("u", 1e-6), ("n", 1e-9), ("p", 1e-12), ("f", 1e-15),
    ("a", 1e-18), ("z", 1e-21), ("y", 1e-24),
];
const BINARY: &[(&str, f64)] = &[
    ("ki", 1024.0), ("Mi", 1_048_576.0), ("Gi", 1_073_741_824.0), ("Ti", 1_099_511_627_776.0),
    ("Pi", 1_125_899_906_842_624.0), ("Ei", 1_152_921_504_606_846_976.0), ("Zi", 1_180_591_620_717_411_303_424.0),
    ("Yi", 1_208_925_819_614_629_174_706_176.0),
];

/// The unit a name means and the prefix's factor (1 without one).
fn find(name: &str) -> Option<(&'static Unit, f64)> {
    if let Some(u) = UNITS.iter().find(|u| u.names.contains(&name)) {
        return Some((u, 1.0));
    }
    let info = |rest: &str| UNITS.iter().find(|u| u.kind == Kind::Information && u.names.contains(&rest));
    for (p, f) in BINARY {
        if let Some(u) = name.strip_prefix(p).and_then(info) {
            return Some((u, *f));
        }
    }
    for (p, f) in PREFIXES {
        let Some(rest) = name.strip_prefix(p) else { continue };
        if let Some(u) = UNITS.iter().find(|u| u.prefix && u.names.contains(&rest)) {
            return Some((u, f.powi(u.power)));
        }
    }
    None
}

fn to_kelvin(name: &str, x: f64) -> f64 {
    match name {
        "C" | "cel" => x + 273.15,
        "F" | "fah" => (x - 32.0) * 5.0 / 9.0 + 273.15,
        "Rank" => x * 5.0 / 9.0,
        "Reau" => x * 1.25 + 273.15,
        _ => x,
    }
}
fn from_kelvin(name: &str, k: f64) -> f64 {
    match name {
        "C" | "cel" => k - 273.15,
        "F" | "fah" => (k - 273.15) * 9.0 / 5.0 + 32.0,
        "Rank" => k * 9.0 / 5.0,
        "Reau" => (k - 273.15) * 0.8,
        _ => k,
    }
}

/// CONVERT(number, from_unit, to_unit): #N/A for an unknown unit or two
/// of different kinds.
pub(super) fn convert(c: &Ctx, a: &[Expr]) -> Ev {
    let x = match c.number(&c.scalar(&a[0])) {
        Ok(x) => x,
        Err(k) => return Ev::err(k),
    };
    let names = (to_text(&c.scalar(&a[1])), to_text(&c.scalar(&a[2])));
    let (from, to) = match names {
        (Ok(f), Ok(t)) => (f, t),
        (Err(k), _) | (_, Err(k)) => return Ev::err(k),
    };
    let (Some((fu, fp)), Some((tu, tp))) = (find(&from), find(&to)) else { return Ev::err(ErrorKind::NA) };
    if fu.kind != tu.kind {
        return Ev::err(ErrorKind::NA);
    }
    let r = if fu.kind == Kind::Temperature {
        // A prefix only goes on kelvins, where it scales.
        let k = to_kelvin(fu.names[0], x * fp);
        from_kelvin(tu.names[0], k) / tp
    } else {
        x * (fu.size * fp) / (tu.size * tp)
    };
    if r.is_finite() { Ev::V(Value::num(r)) } else { Ev::err(ErrorKind::Num) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_name_is_found_once() {
        let mut seen = std::collections::HashSet::new();
        for u in UNITS {
            for n in u.names {
                assert!(seen.insert(*n), "{n} twice");
                assert!(find(n).is_some_and(|(f, p)| std::ptr::eq(f, u) && p == 1.0), "{n}");
            }
        }
        assert!(find("km").is_some_and(|(u, p)| u.names[0] == "m" && p == 1e3));
        assert!(find("cm2").is_some_and(|(u, p)| u.names[0] == "m2" && (p - 1e-4).abs() < 1e-20));
        assert!(find("kibyte").is_some_and(|(u, p)| u.names[0] == "byte" && p == 1024.0));
        assert!(find("kft").is_none());
    }
}

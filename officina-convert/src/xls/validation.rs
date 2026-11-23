//! BIFF8 Dv records (MS-XLS §2.4.95).

use std::sync::Arc;

use officina_grid::condfmt::CmpOp;
use officina_grid::formula::Expr;
use officina_grid::validation::{Allow, ErrorStyle, Message, Validation};
use officina_grid::{Rect, Workbook};

use super::{biff::Cur, formula, Record};

pub(super) fn read(wb: &Workbook, sheet: u16, rec: &Record, ctx: &formula::Ctx) -> Result<(Validation, bool), &'static str> {
    let mut c = Cur::new(rec);
    let malformed = |_| "malformed BIFF8 data validation rules";
    let flags = c.u32().map_err(malformed)?;
    let allow = *Allow::ALL.get((flags & 15) as usize).ok_or("unknown BIFF8 data validation types")?;
    let style = *ErrorStyle::ALL.get((flags >> 4 & 7) as usize).ok_or("unknown BIFF8 data validation alert styles")?;
    let compares = !matches!(allow, Allow::Any | Allow::List | Allow::Custom);
    let op = if compares {
        *CmpOp::ALL.get((flags >> 20 & 15) as usize).ok_or("unknown BIFF8 data validation comparisons")?
    } else {
        CmpOp::Between
    };
    let prompt_title = message(&mut c, 32).map_err(malformed)?;
    let error_title = message(&mut c, 32).map_err(malformed)?;
    let prompt = message(&mut c, 255).map_err(malformed)?;
    let error = message(&mut c, 255).map_err(malformed)?;
    let first = tokens(&mut c).map_err(malformed)?;
    let second = tokens(&mut c).map_err(malformed)?;
    let count = c.u16().map_err(malformed)? as usize;
    if !(1..=432).contains(&count) || c.left() != count * 8 {
        return Err("BIFF8 data validation rules with invalid ranges");
    }
    let mut ranges = Vec::with_capacity(count);
    for _ in 0..count {
        let r0 = c.u16().map_err(malformed)? as u32;
        let r1 = c.u16().map_err(malformed)? as u32;
        let c0 = c.u16().map_err(malformed)? as u32;
        let c1 = c.u16().map_err(malformed)? as u32;
        if r0 > r1 || c0 > c1 || c1 >= formula::COLS {
            return Err("BIFF8 data validation rules with invalid ranges");
        }
        ranges.push(Rect { sheet, r0, r1, c0, c1 });
    }
    let origin = (ranges[0].r0, ranges[0].c0);
    let expression = |tokens| {
        formula::decompile(tokens, &[], origin.0, origin.1, true, ctx)
            .ok()
            .and_then(|t| wb.validation_formula(&t, sheet, origin).ok())
            .ok_or("BIFF8 data validation rules with unsupported formulas")
    };
    let f1 = match allow {
        Allow::Any => None,
        Allow::List if flags & (1 << 7) != 0 => {
            let mut t = Cur::of(first);
            if t.u8().map_err(malformed)? != 0x17 {
                return Err("BIFF8 data validation rules with invalid literal lists");
            }
            let text = t.short_string().map_err(malformed)?;
            if t.left() != 0 || text.is_empty() {
                return Err("BIFF8 data validation rules with invalid literal lists");
            }
            Some(Arc::new(Expr::Array(vec![text.split('\0').map(|item| Expr::Str(item.into())).collect()])))
        }
        _ => Some(expression(first)?),
    };
    let f2 = if compares && matches!(op, CmpOp::Between | CmpOp::NotBetween) {
        Some(expression(second)?)
    } else {
        None
    };
    let mut rule = Validation::new(sheet, ranges, allow, op, f1, f2);
    rule.style = style;
    rule.allow_blank = flags & (1 << 8) != 0;
    rule.dropdown = flags & (1 << 9) == 0;
    rule.show_prompt = flags & (1 << 18) != 0;
    rule.show_error = flags & (1 << 19) != 0;
    rule.prompt = Message::new(&prompt_title, &prompt);
    rule.error = Message::new(&error_title, &error);
    Ok((rule, flags >> 10 & 0xff != 0))
}

fn message(c: &mut Cur<'_>, max: usize) -> Result<String, super::Short> {
    let len = c.u16()? as usize;
    if len > max {
        return Err(super::Short);
    }
    let high = c.u8()? & 1 != 0;
    let text = c.chars(len, high)?;
    // Excel uses a single NUL as the default prompt or alert text.
    Ok(if text == "\0" { String::new() } else { text })
}

fn tokens<'a>(c: &mut Cur<'a>) -> Result<&'a [u8], super::Short> {
    let len = c.u16()? as usize;
    c.skip(2)?;
    c.bytes(len)
}

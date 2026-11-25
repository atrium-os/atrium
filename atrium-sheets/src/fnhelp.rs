//! Function argument tips while a formula is typed (Excel's ScreenTip):
//! the arguments of the function the caret is in, the current one marked.

/// Each function's arguments as Excel names them: `[x]` optional, `...`
/// more of the ones before it.
const ARGS: &[(&str, &str)] = &[
    ("SUM", "number1, [number2], ..."),
    ("PRODUCT", "number1, [number2], ..."),
    ("AVERAGE", "number1, [number2], ..."),
    ("MIN", "number1, [number2], ..."),
    ("MAX", "number1, [number2], ..."),
    ("COUNT", "value1, [value2], ..."),
    ("COUNTA", "value1, [value2], ..."),
    ("COUNTBLANK", "range"),
    ("ABS", "number"),
    ("ROUND", "number, num_digits"),
    ("ROUNDUP", "number, num_digits"),
    ("ROUNDDOWN", "number, num_digits"),
    ("INT", "number"),
    ("TRUNC", "number, [num_digits]"),
    ("MOD", "number, divisor"),
    ("POWER", "number, power"),
    ("SQRT", "number"),
    ("EXP", "number"),
    ("LN", "number"),
    ("LOG", "number, [base]"),
    ("LOG10", "number"),
    ("PI", ""),
    ("SIGN", "number"),
    ("SUMPRODUCT", "array1, [array2], ..."),
    ("SUMIF", "range, criteria, [sum_range]"),
    ("SUMIFS", "sum_range, criteria_range1, criteria1, [criteria_range2], [criteria2], ..."),
    ("COUNTIF", "range, criteria"),
    ("COUNTIFS", "criteria_range1, criteria1, [criteria_range2], [criteria2], ..."),
    ("AVERAGEIF", "range, criteria, [average_range]"),
    ("AVERAGEIFS", "average_range, criteria_range1, criteria1, [criteria_range2], [criteria2], ..."),
    ("RAND", ""),
    ("RANDBETWEEN", "bottom, top"),
    ("MEDIAN", "number1, [number2], ..."),
    ("LARGE", "array, k"),
    ("SMALL", "array, k"),
    ("SUBTOTAL", "function_num, ref1, [ref2], ..."),
    ("STDEV", "number1, [number2], ..."),
    ("STDEV.S", "number1, [number2], ..."),
    ("STDEV.P", "number1, [number2], ..."),
    ("STDEVP", "number1, [number2], ..."),
    ("VAR", "number1, [number2], ..."),
    ("VAR.S", "number1, [number2], ..."),
    ("VAR.P", "number1, [number2], ..."),
    ("VARP", "number1, [number2], ..."),
    ("IF", "logical_test, [value_if_true], [value_if_false]"),
    ("IFS", "logical_test1, value_if_true1, [logical_test2], [value_if_true2], ..."),
    ("IFERROR", "value, value_if_error"),
    ("HYPERLINK", "link_location, [friendly_name]"),
    ("IFNA", "value, value_if_na"),
    ("AND", "logical1, [logical2], ..."),
    ("OR", "logical1, [logical2], ..."),
    ("XOR", "logical1, [logical2], ..."),
    ("NOT", "logical"),
    ("TRUE", ""),
    ("FALSE", ""),
    ("FORMULATEXT", "reference"),
    ("ISBLANK", "value"),
    ("ISNUMBER", "value"),
    ("ISTEXT", "value"),
    ("ISLOGICAL", "value"),
    ("ISERROR", "value"),
    ("ISERR", "value"),
    ("ISNA", "value"),
    ("NA", ""),
    ("ROW", "[reference]"),
    ("COLUMN", "[reference]"),
    ("ROWS", "array"),
    ("COLUMNS", "array"),
    ("LEN", "text"),
    ("LEFT", "text, [num_chars]"),
    ("RIGHT", "text, [num_chars]"),
    ("MID", "text, start_num, num_chars"),
    ("UPPER", "text"),
    ("LOWER", "text"),
    ("TRIM", "text"),
    ("CONCATENATE", "text1, [text2], ..."),
    ("CONCAT", "text1, [text2], ..."),
    ("TEXTJOIN", "delimiter, ignore_empty, text1, [text2], ..."),
    ("REPT", "text, number_times"),
    ("FIND", "find_text, within_text, [start_num]"),
    ("SEARCH", "find_text, within_text, [start_num]"),
    ("SUBSTITUTE", "text, old_text, new_text, [instance_num]"),
    ("VALUE", "text"),
    ("TEXT", "value, format_text"),
    ("EXACT", "text1, text2"),
    ("TEXTAFTER", "text, delimiter, [instance_num], [match_mode], [match_end], [if_not_found]"),
    ("CHOOSE", "index_num, value1, [value2], ..."),
    ("INDEX", "array, row_num, [column_num]"),
    ("MATCH", "lookup_value, lookup_array, [match_type]"),
    ("VLOOKUP", "lookup_value, table_array, col_index_num, [range_lookup]"),
    ("HLOOKUP", "lookup_value, table_array, row_index_num, [range_lookup]"),
    ("XLOOKUP", "lookup_value, lookup_array, return_array, [if_not_found], [match_mode], [search_mode]"),
    ("DATE", "year, month, day"),
    ("DATEVALUE", "date_text"),
    ("EOMONTH", "start_date, months"),
    ("YEAR", "serial_number"),
    ("MONTH", "serial_number"),
    ("DAY", "serial_number"),
    ("WEEKDAY", "serial_number, [return_type]"),
    ("TODAY", ""),
    ("NOW", ""),
    ("SEQUENCE", "rows, [columns], [start], [step]"),
    ("RANDARRAY", "[rows], [columns], [min], [max], [whole_number]"),
    ("TRANSPOSE", "array"),
    ("FILTER", "array, include, [if_empty]"),
    ("SORT", "array, [sort_index], [sort_order], [by_col]"),
    ("SORTBY", "array, by_array1, [sort_order1], ..."),
    ("UNIQUE", "array, [by_col], [exactly_once]"),
    ("VSTACK", "array1, [array2], ..."),
    ("HSTACK", "array1, [array2], ..."),
    ("TAKE", "array, rows, [columns]"),
    ("DROP", "array, rows, [columns]"),
    ("CHOOSEROWS", "array, row_num1, [row_num2], ..."),
    ("CHOOSECOLS", "array, col_num1, [col_num2], ..."),
    ("TOCOL", "array, [ignore], [scan_by_column]"),
    ("TOROW", "array, [ignore], [scan_by_column]"),
    ("WRAPROWS", "vector, wrap_count, [pad_with]"),
    ("WRAPCOLS", "vector, wrap_count, [pad_with]"),
    ("EXPAND", "array, rows, [columns], [pad_with]"),
    ("LET", "name1, name_value1, calculation_or_name2, ..."),
    ("LAMBDA", "[parameter1], ..., calculation"),
    ("MAP", "array1, lambda_or_array2, ..."),
    ("REDUCE", "[initial_value], array, lambda"),
    ("SCAN", "[initial_value], array, lambda"),
    ("BYROW", "array, lambda"),
    ("BYCOL", "array, lambda"),
    ("MAKEARRAY", "rows, cols, lambda"),
    ("ISOMITTED", "argument"),
    ("CEILING", "number, significance"),
    ("FLOOR", "number, significance"),
    ("CEILING.MATH", "number, [significance], [mode]"),
    ("FLOOR.MATH", "number, [significance], [mode]"),
    ("MROUND", "number, multiple"),
    ("QUOTIENT", "numerator, denominator"),
    ("EVEN", "number"),
    ("ODD", "number"),
    ("FACT", "number"),
    ("COMBIN", "number, number_chosen"),
    ("GCD", "number1, [number2], ..."),
    ("LCM", "number1, [number2], ..."),
    ("SUMSQ", "number1, [number2], ..."),
    ("RADIANS", "angle"),
    ("DEGREES", "angle"),
    ("SIN", "number"),
    ("COS", "number"),
    ("TAN", "number"),
    ("ASIN", "number"),
    ("ACOS", "number"),
    ("ATAN", "number"),
    ("ATAN2", "x_num, y_num"),
    ("TIME", "hour, minute, second"),
    ("HOUR", "serial_number"),
    ("MINUTE", "serial_number"),
    ("SECOND", "serial_number"),
    ("EDATE", "start_date, months"),
    ("DAYS", "end_date, start_date"),
    ("DATEDIF", "start_date, end_date, unit"),
    ("NETWORKDAYS", "start_date, end_date, [holidays]"),
    ("WORKDAY", "start_date, days, [holidays]"),
    ("WEEKNUM", "serial_number, [return_type]"),
    ("ISOWEEKNUM", "date"),
    ("PROPER", "text"),
    ("CHAR", "number"),
    ("CODE", "text"),
    ("UNICHAR", "number"),
    ("UNICODE", "text"),
    ("CLEAN", "text"),
    ("SWITCH", "expression, value1, result1, [default_or_value2], [result2], ..."),
    ("ISEVEN", "number"),
    ("ISODD", "number"),
    ("N", "value"),
    ("T", "value"),
    ("MAXIFS", "max_range, criteria_range1, criteria1, [criteria_range2], [criteria2], ..."),
    ("MINIFS", "min_range, criteria_range1, criteria1, [criteria_range2], [criteria2], ..."),
    ("RANK", "number, ref, [order]"),
    ("RANK.EQ", "number, ref, [order]"),
    ("RANK.AVG", "number, ref, [order]"),
    ("PERCENTILE", "array, k"),
    ("PERCENTILE.INC", "array, k"),
    ("PERCENTILE.EXC", "array, k"),
    ("QUARTILE", "array, quart"),
    ("QUARTILE.INC", "array, quart"),
    ("QUARTILE.EXC", "array, quart"),
    ("MODE", "number1, [number2], ..."),
    ("MODE.SNGL", "number1, [number2], ..."),
    ("XMATCH", "lookup_value, lookup_array, [match_mode], [search_mode]"),
    ("PMT", "rate, nper, pv, [fv], [type]"),
    ("IPMT", "rate, per, nper, pv, [fv], [type]"),
    ("PPMT", "rate, per, nper, pv, [fv], [type]"),
    ("FV", "rate, nper, pmt, [pv], [type]"),
    ("PV", "rate, nper, pmt, [fv], [type]"),
    ("NPER", "rate, pmt, pv, [fv], [type]"),
    ("RATE", "nper, pmt, pv, [fv], [type], [guess]"),
    ("NPV", "rate, value1, [value2], ..."),
    ("IRR", "values, [guess]"),
    ("TEXTBEFORE", "text, delimiter, [instance_num], [match_mode], [match_end], [if_not_found]"),
    ("TEXTSPLIT", "text, col_delimiter, [row_delimiter], [ignore_empty], [match_mode], [pad_with]"),
    ("FIXED", "number, [decimals], [no_commas]"),
    ("LOOKUP", "lookup_value, lookup_vector, [result_vector]"),
    ("ADDRESS", "row_num, column_num, [abs_num], [a1], [sheet_text]"),
    ("ISREF", "value"),
    ("YEARFRAC", "start_date, end_date, [basis]"),
    ("SLN", "cost, salvage, life"),
    ("SYD", "cost, salvage, life, per"),
    ("DDB", "cost, salvage, life, period, [factor]"),
    ("DB", "cost, salvage, life, period, [month]"),
    ("INDIRECT", "ref_text, [a1]"),
    ("OFFSET", "reference, rows, cols, [height], [width]"),
    ("AVEDEV", "number1, [number2], ..."),
    ("DEVSQ", "number1, [number2], ..."),
    ("GEOMEAN", "number1, [number2], ..."),
    ("HARMEAN", "number1, [number2], ..."),
    ("STANDARDIZE", "x, mean, standard_dev"),
    ("CORREL", "array1, array2"),
    ("PEARSON", "array1, array2"),
    ("RSQ", "known_ys, known_xs"),
    ("SLOPE", "known_ys, known_xs"),
    ("INTERCEPT", "known_ys, known_xs"),
    ("FORECAST", "x, known_ys, known_xs"),
    ("FORECAST.LINEAR", "x, known_ys, known_xs"),
    ("STEYX", "known_ys, known_xs"),
    ("COVAR", "array1, array2"),
    ("COVARIANCE.P", "array1, array2"),
    ("COVARIANCE.S", "array1, array2"),
    ("MODE.MULT", "number1, [number2], ..."),
    ("MAXA", "value1, [value2], ..."),
    ("MINA", "value1, [value2], ..."),
    ("AVERAGEA", "value1, [value2], ..."),
    ("SINH", "number"),
    ("COSH", "number"),
    ("TANH", "number"),
    ("ASINH", "number"),
    ("ACOSH", "number"),
    ("ATANH", "number"),
    ("REPLACE", "old_text, start_num, num_chars, new_text"),
    ("DOLLAR", "number, [decimals]"),
    ("NUMBERVALUE", "text, [decimal_separator], [group_separator]"),
    ("ROMAN", "number, [form]"),
    ("ARABIC", "text"),
    ("BASE", "number, radix, [min_length]"),
    ("DECIMAL", "text, radix"),
    ("BIN2DEC", "number"),
    ("BIN2HEX", "number, [places]"),
    ("BIN2OCT", "number, [places]"),
    ("DEC2BIN", "number, [places]"),
    ("DEC2HEX", "number, [places]"),
    ("DEC2OCT", "number, [places]"),
    ("HEX2BIN", "number, [places]"),
    ("HEX2DEC", "number"),
    ("HEX2OCT", "number, [places]"),
    ("OCT2BIN", "number, [places]"),
    ("OCT2DEC", "number"),
    ("OCT2HEX", "number, [places]"),
    ("TYPE", "value"),
    ("ERROR.TYPE", "error_val"),
    ("SHEET", "[value]"),
    ("SHEETS", "[reference]"),
    ("ISFORMULA", "reference"),
    ("ISNONTEXT", "value"),
    ("NORM.DIST", "x, mean, standard_dev, cumulative"),
    ("NORMDIST", "x, mean, standard_dev, cumulative"),
    ("NORM.INV", "probability, mean, standard_dev"),
    ("NORMINV", "probability, mean, standard_dev"),
    ("NORM.S.DIST", "z, cumulative"),
    ("NORMSDIST", "z"),
    ("NORM.S.INV", "probability"),
    ("NORMSINV", "probability"),
    ("PHI", "x"),
    ("GAUSS", "z"),
    ("LOGNORM.DIST", "x, mean, standard_dev, cumulative"),
    ("LOGNORMDIST", "x, mean, standard_dev"),
    ("LOGNORM.INV", "probability, mean, standard_dev"),
    ("LOGINV", "probability, mean, standard_dev"),
    ("T.DIST", "x, deg_freedom, cumulative"),
    ("T.DIST.2T", "x, deg_freedom"),
    ("T.DIST.RT", "x, deg_freedom"),
    ("TDIST", "x, deg_freedom, tails"),
    ("T.INV", "probability, deg_freedom"),
    ("T.INV.2T", "probability, deg_freedom"),
    ("TINV", "probability, deg_freedom"),
    ("CHISQ.DIST", "x, deg_freedom, cumulative"),
    ("CHISQ.DIST.RT", "x, deg_freedom"),
    ("CHIDIST", "x, deg_freedom"),
    ("CHISQ.INV", "probability, deg_freedom"),
    ("CHISQ.INV.RT", "probability, deg_freedom"),
    ("CHIINV", "probability, deg_freedom"),
    ("F.DIST", "x, deg_freedom1, deg_freedom2, cumulative"),
    ("F.DIST.RT", "x, deg_freedom1, deg_freedom2"),
    ("FDIST", "x, deg_freedom1, deg_freedom2"),
    ("F.INV", "probability, deg_freedom1, deg_freedom2"),
    ("F.INV.RT", "probability, deg_freedom1, deg_freedom2"),
    ("FINV", "probability, deg_freedom1, deg_freedom2"),
    ("BINOM.DIST", "number_s, trials, probability_s, cumulative"),
    ("BINOMDIST", "number_s, trials, probability_s, cumulative"),
    ("BINOM.DIST.RANGE", "trials, probability_s, number_s, [number_s2]"),
    ("BINOM.INV", "trials, probability_s, alpha"),
    ("CRITBINOM", "trials, probability_s, alpha"),
    ("NEGBINOM.DIST", "number_f, number_s, probability_s, cumulative"),
    ("NEGBINOMDIST", "number_f, number_s, probability_s"),
    ("POISSON.DIST", "x, mean, cumulative"),
    ("POISSON", "x, mean, cumulative"),
    ("HYPGEOM.DIST", "sample_s, number_sample, population_s, number_pop, cumulative"),
    ("HYPGEOMDIST", "sample_s, number_sample, population_s, number_pop"),
    ("EXPON.DIST", "x, lambda, cumulative"),
    ("EXPONDIST", "x, lambda, cumulative"),
    ("GAMMA.DIST", "x, alpha, beta, cumulative"),
    ("GAMMADIST", "x, alpha, beta, cumulative"),
    ("GAMMA.INV", "probability, alpha, beta"),
    ("GAMMAINV", "probability, alpha, beta"),
    ("GAMMA", "number"),
    ("GAMMALN", "x"),
    ("GAMMALN.PRECISE", "x"),
    ("BETA.DIST", "x, alpha, beta, cumulative, [A], [B]"),
    ("BETADIST", "x, alpha, beta, [A], [B]"),
    ("BETA.INV", "probability, alpha, beta, [A], [B]"),
    ("BETAINV", "probability, alpha, beta, [A], [B]"),
    ("WEIBULL.DIST", "x, alpha, beta, cumulative"),
    ("WEIBULL", "x, alpha, beta, cumulative"),
    ("ERF", "lower_limit, [upper_limit]"),
    ("ERF.PRECISE", "x"),
    ("ERFC", "x"),
    ("ERFC.PRECISE", "x"),
    ("FISHER", "x"),
    ("FISHERINV", "y"),
    ("CONFIDENCE.NORM", "alpha, standard_dev, size"),
    ("CONFIDENCE", "alpha, standard_dev, size"),
    ("CONFIDENCE.T", "alpha, standard_dev, size"),
    ("T.TEST", "array1, array2, tails, type"),
    ("TTEST", "array1, array2, tails, type"),
    ("Z.TEST", "array, x, [sigma]"),
    ("ZTEST", "array, x, [sigma]"),
    ("CHISQ.TEST", "actual_range, expected_range"),
    ("CHITEST", "actual_range, expected_range"),
    ("F.TEST", "array1, array2"),
    ("FTEST", "array1, array2"),
    ("LINEST", "known_ys, [known_xs], [const], [stats]"),
    ("LOGEST", "known_ys, [known_xs], [const], [stats]"),
    ("TREND", "known_ys, [known_xs], [new_xs], [const]"),
    ("GROWTH", "known_ys, [known_xs], [new_xs], [const]"),
    ("COUPPCD", "settlement, maturity, frequency, [basis]"),
    ("COUPNCD", "settlement, maturity, frequency, [basis]"),
    ("COUPNUM", "settlement, maturity, frequency, [basis]"),
    ("COUPDAYS", "settlement, maturity, frequency, [basis]"),
    ("COUPDAYBS", "settlement, maturity, frequency, [basis]"),
    ("COUPDAYSNC", "settlement, maturity, frequency, [basis]"),
    ("PRICE", "settlement, maturity, rate, yld, redemption, frequency, [basis]"),
    ("YIELD", "settlement, maturity, rate, pr, redemption, frequency, [basis]"),
    ("DURATION", "settlement, maturity, coupon, yld, frequency, [basis]"),
    ("MDURATION", "settlement, maturity, coupon, yld, frequency, [basis]"),
    ("ACCRINT", "issue, first_interest, settlement, rate, par, frequency, [basis], [calc_method]"),
    ("ACCRINTM", "issue, settlement, rate, par, [basis]"),
    ("DISC", "settlement, maturity, pr, redemption, [basis]"),
    ("PRICEDISC", "settlement, maturity, discount, redemption, [basis]"),
    ("YIELDDISC", "settlement, maturity, pr, redemption, [basis]"),
    ("RECEIVED", "settlement, maturity, investment, discount, [basis]"),
    ("INTRATE", "settlement, maturity, investment, redemption, [basis]"),
    ("PRICEMAT", "settlement, maturity, issue, rate, yld, [basis]"),
    ("YIELDMAT", "settlement, maturity, issue, rate, pr, [basis]"),
    ("TBILLPRICE", "settlement, maturity, discount"),
    ("TBILLYIELD", "settlement, maturity, pr"),
    ("TBILLEQ", "settlement, maturity, discount"),
    ("XNPV", "rate, values, dates"),
    ("XIRR", "values, dates, [guess]"),
    ("MIRR", "values, finance_rate, reinvest_rate"),
    ("CUMIPMT", "rate, nper, pv, start_period, end_period, type"),
    ("CUMPRINC", "rate, nper, pv, start_period, end_period, type"),
    ("EFFECT", "nominal_rate, npery"),
    ("NOMINAL", "effect_rate, npery"),
    ("FVSCHEDULE", "principal, schedule"),
    ("PDURATION", "rate, pv, fv"),
    ("RRI", "nper, pv, fv"),
    ("ISPMT", "rate, per, nper, pv"),
    ("DOLLARDE", "fractional_dollar, fraction"),
    ("DOLLARFR", "decimal_dollar, fraction"),
    ("VDB", "cost, salvage, life, start_period, end_period, [factor], [no_switch]"),
    ("CELL", "info_type, [reference]"),
    ("INFO", "type_text"),
    ("MMULT", "array1, array2"),
    ("MINVERSE", "array"),
    ("MDETERM", "array"),
    ("MUNIT", "dimension"),
    ("COMPLEX", "real_num, i_num, [suffix]"),
    ("IMSUM", "inumber1, [inumber2], ..."),
    ("IMPRODUCT", "inumber1, [inumber2], ..."),
    ("IMSUB", "inumber1, inumber2"),
    ("IMDIV", "inumber1, inumber2"),
    ("IMPOWER", "inumber, number"),
    ("BITLSHIFT", "number, shift_amount"),
    ("BITRSHIFT", "number, shift_amount"),
    ("DELTA", "number1, [number2]"),
    ("GESTEP", "number, [step]"),
    ("PERMUT", "number, number_chosen"),
    ("PERMUTATIONA", "number, number_chosen"),
    ("COMBINA", "number, number_chosen"),
    ("FACTDOUBLE", "number"),
    ("MULTINOMIAL", "number1, [number2], ..."),
    ("SERIESSUM", "x, n, m, coefficients"),
    ("SQRTPI", "number"),
    ("SUMX2MY2", "array_x, array_y"),
    ("SUMX2PY2", "array_x, array_y"),
    ("SUMXMY2", "array_x, array_y"),
    ("CEILING.PRECISE", "number, [significance]"),
    ("ISO.CEILING", "number, [significance]"),
    ("FLOOR.PRECISE", "number, [significance]"),
    ("KURT", "number1, [number2], ..."),
    ("SKEW", "number1, [number2], ..."),
    ("SKEW.P", "number1, [number2], ..."),
    ("PERCENTRANK", "array, x, [significance]"),
    ("PERCENTRANK.INC", "array, x, [significance]"),
    ("PERCENTRANK.EXC", "array, x, [significance]"),
    ("TRIMMEAN", "array, percent"),
    ("FREQUENCY", "data_array, bins_array"),
    ("PROB", "x_range, prob_range, lower_limit, [upper_limit]"),
    ("STDEVA", "value1, [value2], ..."),
    ("STDEVPA", "value1, [value2], ..."),
    ("VARA", "value1, [value2], ..."),
    ("VARPA", "value1, [value2], ..."),
    ("AGGREGATE", "function_num, options, ref1, [ref2], ..."),
    ("DAYS360", "start_date, end_date, [method]"),
    ("WORKDAY.INTL", "start_date, days, [weekend], [holidays]"),
    ("NETWORKDAYS.INTL", "start_date, end_date, [weekend], [holidays]"),
    ("TIMEVALUE", "time_text"),
    ("BITAND", "number1, number2"),
    ("BITOR", "number1, number2"),
    ("BITXOR", "number1, number2"),
    ("IMREAL", "inumber"),
    ("IMAGINARY", "inumber"),
    ("IMABS", "inumber"),
    ("IMARGUMENT", "inumber"),
    ("IMCONJUGATE", "inumber"),
    ("IMSQRT", "inumber"),
    ("IMEXP", "inumber"),
    ("IMLN", "inumber"),
    ("IMLOG10", "inumber"),
    ("IMLOG2", "inumber"),
    ("IMSIN", "inumber"),
    ("IMCOS", "inumber"),
    ("IMTAN", "inumber"),
    ("IMCOT", "inumber"),
    ("IMSEC", "inumber"),
    ("IMCSC", "inumber"),
    ("IMSINH", "inumber"),
    ("IMCOSH", "inumber"),
    ("IMSECH", "inumber"),
    ("IMCSCH", "inumber"),
    ("COT", "number"),
    ("COTH", "number"),
    ("CSC", "number"),
    ("CSCH", "number"),
    ("SEC", "number"),
    ("SECH", "number"),
    ("ACOT", "number"),
    ("ACOTH", "number"),
];

/// The most characters of a formula the tip reads.
pub const MAX_TIP_SCAN: usize = 8192;

/// The argument names of `func` (any case), or None for a function not listed.
pub fn args(func: &str) -> Option<Vec<&'static str>> {
    let f = func.strip_prefix("_xlfn.").unwrap_or(func);
    let (_, a) = ARGS.iter().find(|(n, _)| n.eq_ignore_ascii_case(f))?;
    Some(if a.is_empty() { Vec::new() } else { a.split(", ").collect() })
}

/// Formula AutoComplete: `typed` (a formula, the caret at its end) with the
/// name being typed finished, when exactly one function (then `(`) or
/// defined name begins with it. Not inside a string or a quoted sheet name,
/// and not after a sheet name (`Sheet1!A`) or a digit.
pub fn complete_formula(typed: &str, names: &[String]) -> Option<String> {
    if !typed.starts_with('=') || typed.len() > MAX_TIP_SCAN {
        return None;
    }
    // Inside quotes, nothing.
    let mut quote = None;
    for ch in typed.chars() {
        match quote {
            Some(q) if ch == q => quote = None,
            Some(_) => {}
            None if ch == '"' || ch == '\'' => quote = Some(ch),
            None => {}
        }
    }
    if quote.is_some() {
        return None;
    }
    let start = typed.rfind(|c: char| !(c.is_ascii_alphanumeric() || c == '.' || c == '_')).map_or(0, |k| k + 1);
    let word = &typed[start..];
    let before = typed[..start].chars().last();
    if word.is_empty() || !word.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_') || matches!(before, Some('!' | ':' | '$')) {
        return None;
    }
    let w = word.to_ascii_lowercase();
    let funcs = officina_grid::eval::funcs::FUNCS.iter().map(|f| f.name).filter(|n| !n.starts_with('_')).filter(|n| n.len() > w.len() && n.to_ascii_lowercase().starts_with(&w)).map(|n| format!("{n}("));
    let names = names.iter().filter(|n| n.len() > w.len() && n.to_lowercase().starts_with(&w)).cloned();
    let mut hits = funcs.chain(names);
    let one = hits.next()?;
    hits.next().is_none().then(|| format!("{}{one}", &typed[..start]))
}

/// The function the caret at byte `caret` of `text` is inside, and which of
/// its arguments (from 0) it is in. None outside a function or a formula.
pub fn call_at(text: &str, caret: usize) -> Option<(String, usize)> {
    if !text.starts_with('=') || caret > MAX_TIP_SCAN || !text.is_char_boundary(caret) {
        return None;
    }
    let head = &text[..caret];
    // Open brackets: a function's name and its commas so far, or None for
    // plain parentheses and array constants.
    let mut open: Vec<Option<(usize, usize, usize)>> = Vec::new();
    let mut quote: Option<char> = None;
    for (i, ch) in head.char_indices() {
        if let Some(q) = quote {
            if ch == q {
                quote = None;
            }
            continue;
        }
        match ch {
            '"' | '\'' => quote = Some(ch),
            '(' => {
                let start = head[..i].rfind(|c: char| !(c.is_ascii_alphanumeric() || c == '.' || c == '_')).map_or(0, |k| k + 1);
                open.push((start < i && head[start..].starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')).then_some((start, i, 0)));
            }
            '{' => open.push(None),
            ')' | '}' => {
                open.pop();
            }
            ',' => {
                if let Some(Some((_, _, n))) = open.last_mut() {
                    *n += 1;
                }
            }
            _ => {}
        }
    }
    if quote == Some('"') {
        return None;
    }
    // In plain parentheses inside a call, the call's argument is the one.
    let &(s, e, n) = open.iter().rev().find_map(|f| f.as_ref())?;
    Some((head[s..e].to_ascii_uppercase(), n))
}

/// The tip for the caret: `NAME(` then each argument, the one the caret is in
/// marked true. None when there's nothing to show.
pub fn tip(text: &str, caret: usize) -> Option<Vec<(String, bool)>> {
    let (name, at) = call_at(text, caret)?;
    let list = args(&name)?;
    let shown = name.strip_prefix("_XLFN.").unwrap_or(&name).to_string();
    // Past the listed ones (a repeating list): the "..." is current.
    let more = list.iter().position(|a| *a == "...");
    let cur = match more {
        Some(k) if at >= k => k,
        _ => at,
    };
    let mut out = vec![(format!("{shown}("), false)];
    for (i, a) in list.iter().enumerate() {
        let sep = if i + 1 < list.len() { ", " } else { "" };
        out.push((a.to_string(), i == cur));
        if !sep.is_empty() {
            out.push((sep.into(), false));
        }
    }
    out.push((")".into(), false));
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_function_has_its_arguments_and_they_match_its_counts() {
        for f in officina_grid::eval::funcs::FUNCS {
            let a = args(f.name).unwrap_or_else(|| panic!("{} has no argument names", f.name));
            let repeats = a.contains(&"...");
            let required = a.iter().filter(|x| !x.starts_with('[') && **x != "...").count();
            assert_eq!(required, f.min as usize, "{}: required arguments", f.name);
            if repeats {
                assert_eq!(f.max, 255, "{}: only a function taking any number repeats", f.name);
            } else {
                assert_eq!(a.len(), f.max as usize, "{}: all arguments", f.name);
            }
        }
        assert_eq!(ARGS.len(), officina_grid::eval::funcs::FUNCS.len(), "no names for functions Sheets lacks");
    }

    #[test]
    fn the_caret_s_function_and_argument_are_found() {
        let at = |t: &str| call_at(t, t.len());
        assert_eq!(at("=VLOOKUP(A1, B:C, "), Some(("VLOOKUP".into(), 2)));
        assert_eq!(at("=vlookup("), Some(("VLOOKUP".into(), 0)));
        // Inner calls, plain parentheses, arrays and strings don't count their commas.
        assert_eq!(at("=IF(SUM(1,2)>3, (1+2"), Some(("IF".into(), 1)));
        assert_eq!(at("=SUM(ROUND(A1, "), Some(("ROUND".into(), 1)));
        assert_eq!(at("=CHOOSE({1,2,3}, \"a,b\", "), Some(("CHOOSE".into(), 2)));
        assert_eq!(at("=SUM('a, b'!A1, "), Some(("SUM".into(), 1)));
        assert_eq!(at("=SUM(1)+"), None);
        assert_eq!(at("=LEN(\"a,("), None, "inside a string");
        assert_eq!(at("SUM("), None, "not a formula");
        assert_eq!(at("=(1+2"), None);
        assert_eq!(call_at("=ROUND(A1, 2)", 7), Some(("ROUND".into(), 0)));
    }

    #[test]
    fn formula_autocomplete_finishes_one_function_or_name() {
        let names = vec!["Costs".to_string(), "Totals2025".to_string()];
        let c = |t: &str| complete_formula(t, &names);
        assert_eq!(c("=vlo").as_deref(), Some("=VLOOKUP("));
        assert_eq!(c("=1+SUMP").as_deref(), Some("=1+SUMPRODUCT("));
        assert_eq!(c("=SUM"), None, "SUMIF, SUMIFS and SUMPRODUCT");
        assert_eq!(c("=co"), None, "functions and a name");
        assert_eq!(c("=cost").as_deref(), Some("=Costs"));
        assert_eq!(c("=cos"), None, "COS, COSH and a name");
        assert_eq!(c("=SUM(tot").as_deref(), Some("=SUM(Totals2025"));
        assert_eq!(c("=Sheet1!XL"), None, "after a sheet name");
        assert_eq!(c("=A1:XL"), None);
        assert_eq!(c("=\"vlo"), None, "in a string");
        assert_eq!(c("=1"), None);
        assert_eq!(c("vlo"), None, "not a formula");
    }

    #[test]
    fn the_tip_marks_the_current_argument() {
        let marked = |t: &str| tip(t, t.len()).map(|v| v.into_iter().filter(|(_, on)| *on).map(|(s, _)| s).collect::<Vec<_>>());
        assert_eq!(marked("=VLOOKUP(A1, B:C, "), Some(vec!["col_index_num".to_string()]));
        assert_eq!(marked("=SUM(1, 2, 3, "), Some(vec!["...".to_string()]));
        assert_eq!(marked("=NOPE("), None);
        let all: String = tip("=MOD(", 5).unwrap().into_iter().map(|(s, _)| s).collect();
        assert_eq!(all, "MOD(number, divisor)");
    }
}

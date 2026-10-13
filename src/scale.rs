//! Log scales, shared by the 2D and 3D views: data are drawn as log10 of the
//! values, and ticks are labelled with the values themselves.

/// log10 of `v`, or `None` when `v` has no place on a log axis (≤ 0 or not finite).
pub fn to_log(v: f64) -> Option<f64> {
  (v > 0.0 && v.is_finite()).then(|| v.log10())
}

/// Tick positions (exponents) for a log axis spanning exponents `lo..=hi`,
/// at most about `max` of them: whole decades when the span covers at least
/// two (every k-th decade if there are too many), else 1, 2 and 5 times each
/// decade, else 1 to 9 times each decade.
pub fn log_ticks(lo: f64, hi: f64, max: usize) -> Vec<f64> {
  let (lo, hi) = if lo <= hi { (lo, hi) } else { (hi, lo) };
  if !lo.is_finite() || !hi.is_finite() || max == 0 {
    return Vec::new();
  }
  let (first, last) = (lo.floor() as i64, hi.ceil() as i64);
  let inside = |e: f64| e >= lo - 1e-9 && e <= hi + 1e-9;
  if hi - lo >= 2.0 {
    let decades = (last - first) as usize + 1;
    let step = decades.div_ceil(max).max(1) as i64;
    let start = first.div_euclid(step) * step;
    return (0..)
      .map(|i| (start + i * step) as f64)
      .take_while(|&e| e <= hi + 1e-9)
      .filter(|&e| inside(e))
      .collect();
  }
  const COARSE: [f64; 3] = [1.0, 2.0, 5.0];
  const FINE: [f64; 9] = [1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0];
  for mantissas in [&COARSE[..], &FINE[..]] {
    let ticks: Vec<f64> = (first..=last)
      .flat_map(|d| mantissas.iter().map(move |m: &f64| d as f64 + m.log10()))
      .filter(|&e| inside(e))
      .collect();
    if ticks.len() >= 3 || mantissas.len() == 9 {
      return ticks;
    }
  }
  unreachable!("the last mantissa set always returns")
}

/// Label for the tick at exponent `e`: the value 10^e with at most three
/// significant digits, plain between 0.001 and 9999 (`0.05`, `1`, `200`),
/// scientific otherwise (`1e-5`, `3.16e7`).
pub fn log_label(e: f64) -> String {
  let v = 10f64.powf(e);
  if !v.is_finite() || v <= 0.0 {
    return String::new();
  }
  // Round to three significant digits first, so that 10^2 = 99.99999… reads "100".
  let magnitude = v.log10().floor() as i32;
  let unit = 10f64.powi(magnitude - 2);
  let v = (v / unit).round() * unit;
  if (1e-3..1e4).contains(&v) {
    let decimals = (2 - v.log10().floor() as i32).max(0) as usize;
    trim_zeros(&format!("{v:.decimals$}"))
  } else {
    let s = format!("{v:.2e}");
    match s.split_once('e') {
      Some((mantissa, exponent)) => format!("{}e{exponent}", trim_zeros(mantissa)),
      None => s,
    }
  }
}

/// Drops trailing zeros after a decimal point, and the point if nothing is left.
fn trim_zeros(s: &str) -> String {
  if s.contains('.') {
    s.trim_end_matches('0').trim_end_matches('.').to_string()
  } else {
    s.to_string()
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn to_log_drops_non_positive_values() {
    assert_eq!(to_log(100.0), Some(2.0));
    assert_eq!(to_log(0.0), None);
    assert_eq!(to_log(-1.0), None);
    assert_eq!(to_log(f64::NAN), None);
    assert_eq!(to_log(f64::INFINITY), None);
  }

  #[test]
  fn labels_read_like_the_values() {
    let cases = [
      (0.0, "1"),
      (1.0, "10"),
      (2.0, "100"),
      (3.0, "1000"),
      (-1.0, "0.1"),
      (-3.0, "0.001"),
      (4.0, "1e4"),
      (7.0, "1e7"),
      (-4.0, "1e-4"),
      (2f64.log10(), "2"),
      (50f64.log10(), "50"),
      (0.05f64.log10(), "0.05"),
      (3.16e7f64.log10(), "3.16e7"),
    ];
    for (e, label) in cases {
      assert_eq!(log_label(e), label, "exponent {e}");
    }
  }

  #[test]
  fn ticks_are_decades_over_a_wide_span() {
    assert_eq!(log_ticks(-0.3, 3.2, 10), vec![0.0, 1.0, 2.0, 3.0]);
    // 31 decades with room for 10 ticks: every 4th decade.
    let t = log_ticks(-15.0, 15.0, 10);
    assert!(t.len() <= 10 && t.len() >= 5, "{t:?}");
    assert!(t.iter().all(|e| e.fract() == 0.0 && *e as i64 % 4 == 0));
  }

  #[test]
  fn ticks_subdivide_narrow_spans() {
    // 1 to 100: 1, 2, 5, 10, 20, 50, 100.
    let t = log_ticks(0.0, 1.99, 10);
    let labels: Vec<String> = t.iter().map(|&e| log_label(e)).collect();
    assert_eq!(labels, ["1", "2", "5", "10", "20", "50"]);
    // Within one decade, 2 to 8: every integer mantissa.
    let t = log_ticks(2f64.log10(), 8f64.log10(), 10);
    let labels: Vec<String> = t.iter().map(|&e| log_label(e)).collect();
    assert_eq!(labels, ["2", "3", "4", "5", "6", "7", "8"]);
  }

  #[test]
  fn ticks_handle_bad_input() {
    assert!(log_ticks(f64::NAN, 1.0, 5).is_empty());
    assert!(log_ticks(0.0, 1.0, 0).is_empty());
    assert_eq!(log_ticks(3.2, -0.3, 10), log_ticks(-0.3, 3.2, 10));
  }
}

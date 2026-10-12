//! Histograms: shared bins for several groups, by Sturges' or Scott's rule.

use egui_plot::PlotPoint;

/// Rule that sets the bins of a histogram.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BinRule {
  /// ⌈log₂ n⌉ + 1 bins.
  #[default]
  Sturges,
  /// Bins of width 3.49 σ n^(-1/3).
  Scott,
}

/// Most bins a histogram gets, whatever its rule asks for.
pub const MAX_BINS: usize = 1000;

/// Counts of several groups over the same bins.
#[derive(Debug, Clone, PartialEq)]
pub struct Histogram {
  /// Left edge of the first bin.
  pub start: f64,
  /// Width of every bin, always > 0.
  pub width: f64,
  /// `counts[g][b]`: number of values of group `g` (in input order) in bin `b`.
  pub counts: Vec<Vec<u64>>,
}

impl Histogram {
  /// Number of bins.
  pub fn bins(&self) -> usize {
    self.counts.first().map_or(0, Vec::len)
  }

  /// Centre of bin `b`.
  pub fn center(&self, b: usize) -> f64 {
    self.start + (b as f64 + 0.5) * self.width
  }
}

/// Bins the y values of all `groups` together, so that they share the same
/// bins. Non-finite values are ignored. `None` if there are no values at all.
pub fn histogram(groups: &[&[PlotPoint]], rule: BinRule) -> Option<Histogram> {
  let Summary { n, min, max, sigma } = summarize(groups)?;
  let (start, width, k) = if max == min {
    (min - 0.5, 1.0, 1)
  } else {
    let k = match rule {
      BinRule::Sturges => sturges_bins(n).clamp(1, MAX_BINS),
      BinRule::Scott => scott_bins(n, sigma, min, max),
    };
    // Keeps the width positive even when a subnormal range underflows.
    (min, ((max - min) / k as f64).max(f64::MIN_POSITIVE), k)
  };
  let counts = groups
    .iter()
    .map(|g| {
      let mut c = vec![0u64; k];
      for p in g.iter().filter(|p| p.y.is_finite()) {
        c[bin_of(p.y, start, width, max, k)] += 1;
      }
      c
    })
    .collect();
  Some(Histogram {
    start,
    width,
    counts,
  })
}

/// Count, extremes and spread of the finite values of all groups.
struct Summary {
  /// Number of finite values.
  n: usize,
  min: f64,
  max: f64,
  /// Sample standard deviation (denominator n - 1), 0 for n = 1.
  sigma: f64,
}

/// One pass over the finite values. The spread uses Welford's update, so it
/// needs no second pass and no copy of the values.
fn summarize(groups: &[&[PlotPoint]]) -> Option<Summary> {
  let (mut n, mut min, mut max) = (0usize, f64::INFINITY, f64::NEG_INFINITY);
  let (mut mean, mut m2) = (0.0, 0.0);
  for p in groups
    .iter()
    .flat_map(|g| g.iter())
    .filter(|p| p.y.is_finite())
  {
    n += 1;
    min = min.min(p.y);
    max = max.max(p.y);
    let delta = p.y - mean;
    mean += delta / n as f64;
    m2 += delta * (p.y - mean);
  }
  if n == 0 {
    return None;
  }
  let sigma = if n > 1 {
    (m2.max(0.0) / (n - 1) as f64).sqrt()
  } else {
    0.0
  };
  Some(Summary { n, min, max, sigma })
}

/// Sturges' bin count for `n >= 1` values, from the bit length of `n - 1`
/// (which is exactly ⌈log₂ n⌉), so that powers of two are exact.
fn sturges_bins(n: usize) -> usize {
  (usize::BITS - (n - 1).leading_zeros()) as usize + 1
}

/// Scott's bin count over `[min, max]` for `n` values of spread `sigma`.
fn scott_bins(n: usize, sigma: f64, min: f64, max: f64) -> usize {
  let h = 3.49 * sigma * (n as f64).powf(-1.0 / 3.0);
  if h <= 0.0 {
    1
  } else {
    clamp_bins(((max - min) / h).ceil())
  }
}

/// Bin count from a real `k`, clamped to `1..=MAX_BINS`. NaN gives one bin.
fn clamp_bins(k: f64) -> usize {
  if k.is_nan() {
    1
  } else {
    k.clamp(1.0, MAX_BINS as f64) as usize
  }
}

/// Bin of `y` among `k` bins of `width` from `start`. `max` goes to the last bin.
fn bin_of(y: f64, start: f64, width: f64, max: f64, k: usize) -> usize {
  if y >= max {
    k - 1
  } else {
    // Saturating cast: negative or NaN give 0, huge values give k - 1.
    (((y - start) / width).floor() as usize).min(k - 1)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Points at x = index, with the given y values.
  fn pts(ys: &[f64]) -> Vec<PlotPoint> {
    ys.iter()
      .enumerate()
      .map(|(i, &y)| PlotPoint { x: i as f64, y })
      .collect()
  }

  #[test]
  fn no_values_gives_none() {
    let empty: &[PlotPoint] = &[];
    let non_finite = pts(&[f64::NAN, f64::INFINITY, f64::NEG_INFINITY]);
    for rule in [BinRule::Sturges, BinRule::Scott] {
      assert!(histogram(&[], rule).is_none());
      assert!(histogram(&[empty, empty], rule).is_none());
      assert!(histogram(&[non_finite.as_slice()], rule).is_none());
    }
  }

  #[test]
  fn sturges_bin_counts() {
    for (n, k) in [
      (1usize, 1usize),
      (2, 2),
      (8, 4),
      (9, 5),
      (100, 8),
      (1000, 11),
    ] {
      let ys: Vec<f64> = (0..n).map(|i| i as f64).collect();
      let p = pts(&ys);
      let h = histogram(&[p.as_slice()], BinRule::Sturges).unwrap();
      assert_eq!(h.bins(), k, "n = {n}");
    }
  }

  #[test]
  fn scott_on_zero_to_99() {
    // Sample variance of 0..n is n (n + 1) / 12.
    let sigma = (100.0_f64 * 101.0 / 12.0).sqrt();
    let h = 3.49 * sigma * 100f64.powf(-1.0 / 3.0);
    let ys: Vec<f64> = (0..100).map(|i| i as f64).collect();
    let p = pts(&ys);
    let s = summarize(&[p.as_slice()]).unwrap();
    assert!((s.sigma - sigma).abs() < 1e-9);
    assert!((sigma - 29.01).abs() < 0.005);
    assert!((h - 21.81).abs() < 0.005);
    assert_eq!(scott_bins(100, sigma, 0.0, 99.0), 5);
    let hist = histogram(&[p.as_slice()], BinRule::Scott).unwrap();
    assert_eq!(hist.bins(), 5);
    assert!((hist.width - 99.0 / 5.0).abs() < 1e-12);
  }

  #[test]
  fn two_groups_share_bins() {
    let a = pts(&[0.0, 1.0, 2.0, 3.0, 10.0]);
    let b = pts(&[5.0, 7.5, 10.0, f64::NAN, 4.0]);
    let h = histogram(&[a.as_slice(), b.as_slice()], BinRule::Sturges).unwrap();
    // Nine finite values: Sturges gives 5 bins of width 2 from 0.
    assert_eq!(h.bins(), 5);
    assert_eq!(h.start, 0.0);
    assert_eq!(h.width, 2.0);
    assert_eq!(h.counts, vec![vec![2, 2, 0, 0, 1], vec![0, 0, 2, 1, 1]]);
    assert_eq!(h.counts[0].iter().sum::<u64>(), 5);
    assert_eq!(h.counts[1].iter().sum::<u64>(), 4);
  }

  #[test]
  fn all_equal_values_give_one_bin() {
    let a = pts(&[3.0, 3.0]);
    let b = pts(&[3.0]);
    for rule in [BinRule::Sturges, BinRule::Scott] {
      let h = histogram(&[a.as_slice(), b.as_slice()], rule).unwrap();
      assert_eq!(h.bins(), 1);
      assert_eq!(h.width, 1.0);
      assert_eq!(h.center(0), 3.0);
      assert_eq!(h.counts, vec![vec![2], vec![1]]);
    }
    // A single value: sigma is 0, so Scott's h is 0 and gives one bin too.
    let one = pts(&[-7.0]);
    let h = histogram(&[one.as_slice()], BinRule::Scott).unwrap();
    assert_eq!(h.bins(), 1);
    assert_eq!(h.center(0), -7.0);
  }

  #[test]
  fn scott_clamps_to_max_bins() {
    // sigma is 1e6 and h is 3.49e4, so 1e9 / h is about 2.9e4 bins before the clamp.
    let mut ys = vec![0.0; 999_999];
    ys.push(1e9);
    let p = pts(&ys);
    let h = histogram(&[p.as_slice()], BinRule::Scott).unwrap();
    assert_eq!(h.bins(), MAX_BINS);
    assert_eq!(h.counts[0][0], 999_999);
    assert_eq!(h.counts[0][MAX_BINS - 1], 1);
    assert_eq!(h.counts[0].iter().sum::<u64>(), 1_000_000);
  }

  #[test]
  fn empty_group_gets_zero_counts() {
    let a = pts(&[1.0, 2.0, 3.0, 4.0]);
    let empty: Vec<PlotPoint> = Vec::new();
    let only_nan = pts(&[f64::NAN]);
    let c = pts(&[5.0]);
    let groups = [
      a.as_slice(),
      empty.as_slice(),
      only_nan.as_slice(),
      c.as_slice(),
    ];
    let h = histogram(&groups, BinRule::Sturges).unwrap();
    // Five values: Sturges gives 4 bins of width 1 from 1.
    assert_eq!(h.bins(), 4);
    assert_eq!(h.counts[0], vec![1, 1, 1, 1]);
    assert_eq!(h.counts[1], vec![0; 4]);
    assert_eq!(h.counts[2], vec![0; 4]);
    assert_eq!(h.counts[3], vec![0, 0, 0, 1]);
  }

  #[test]
  fn bin_centres() {
    let ys: Vec<f64> = (0..=10).map(|i| i as f64).collect();
    let p = pts(&ys);
    // Eleven values: Sturges gives 5 bins of width 2 from 0.
    let h = histogram(&[p.as_slice()], BinRule::Sturges).unwrap();
    assert_eq!(h.bins(), 5);
    assert_eq!(h.counts[0], vec![2, 2, 2, 2, 3]);
    let centres: Vec<f64> = (0..h.bins()).map(|b| h.center(b)).collect();
    assert_eq!(centres, vec![1.0, 3.0, 5.0, 7.0, 9.0]);

    // Two values: 2 bins of width 4 from -4, centred on -2 and 2.
    let p = pts(&[-4.0, 4.0]);
    let h = histogram(&[p.as_slice()], BinRule::Sturges).unwrap();
    assert_eq!(h.center(0), -2.0);
    assert_eq!(h.center(1), 2.0);
  }

  #[test]
  fn speed_sanity_one_million_values_in_four_groups() {
    // Deterministic xorshift, so that the data is the same on every run.
    let mut state = 0x9E37_79B9_7F4A_7C15_u64;
    let mut next = move || {
      state ^= state << 13;
      state ^= state >> 7;
      state ^= state << 17;
      (state >> 11) as f64 / (1u64 << 53) as f64
    };
    let groups: Vec<Vec<PlotPoint>> = (0..4)
      .map(|g| {
        (0..250_000)
          .map(|i| PlotPoint {
            x: i as f64,
            y: 100.0 * next() + g as f64,
          })
          .collect()
      })
      .collect();
    let slices: Vec<&[PlotPoint]> = groups.iter().map(Vec::as_slice).collect();
    let h = histogram(&slices, BinRule::Sturges).unwrap();

    // 1e6 values: ceil(log2 1e6) + 1 = 21 bins.
    assert_eq!(h.bins(), 21);
    for (g, c) in h.counts.iter().enumerate() {
      assert_eq!(c.iter().sum::<u64>(), 250_000, "group {g}");
    }
    let lo = groups
      .iter()
      .flatten()
      .map(|p| p.y)
      .fold(f64::INFINITY, f64::min);
    let hi = groups
      .iter()
      .flatten()
      .map(|p| p.y)
      .fold(f64::NEG_INFINITY, f64::max);
    assert!((h.width * 21.0 - (hi - lo)).abs() < 1e-9);
    assert!(h.counts.iter().any(|c| c[0] > 0));
    assert!(h.counts.iter().any(|c| c[20] > 0));
  }
}

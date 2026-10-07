use crate::matrix::Stat;

#[must_use]
pub fn quantile_sorted(sorted: &[f64], q: f64) -> Option<f64> {
    if sorted.is_empty() || !(0.0..=1.0).contains(&q) {
        return None;
    }
    let pos = (sorted.len() - 1) as f64 * q;
    let lo = pos.floor() as usize;
    let hi = pos.ceil() as usize;
    let frac = pos - lo as f64;
    Some(sorted[lo] + (sorted[hi] - sorted[lo]) * frac)
}

#[must_use]
pub fn sorted(samples: &[f64]) -> Vec<f64> {
    let mut v: Vec<f64> = samples.iter().copied().filter(|x| x.is_finite()).collect();
    v.sort_by(f64::total_cmp);
    v
}

#[must_use]
pub fn quantile(samples: &[f64], q: f64) -> Option<f64> {
    quantile_sorted(&sorted(samples), q)
}

#[must_use]
pub fn stat(samples: &[f64], stat: Stat) -> Option<f64> {
    quantile(samples, stat.quantile())
}

#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Summary {
    pub n: usize,
    pub p50: f64,
    pub p90: f64,
    pub p99: f64,
    pub max: f64,
}

#[must_use]
pub fn summarize(samples: &[f64]) -> Option<Summary> {
    let s = sorted(samples);
    Some(Summary {
        n: s.len(),
        p50: quantile_sorted(&s, 0.5)?,
        p90: quantile_sorted(&s, 0.9)?,
        p99: quantile_sorted(&s, 0.99)?,
        max: *s.last()?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quantiles_interpolate_like_duckdb_quantile_cont() {
        let v = [4.0, 1.0, 3.0, 2.0];
        assert_eq!(quantile(&v, 0.5), Some(2.5));
        assert_eq!(quantile(&v, 0.0), Some(1.0));
        assert_eq!(quantile(&v, 1.0), Some(4.0));
        assert_eq!(quantile(&[], 0.5), None);
        assert_eq!(quantile(&[f64::NAN, 7.0], 0.5), Some(7.0));
    }

    #[test]
    fn a_summary_reports_every_stat_and_the_max() {
        let v: Vec<f64> = (1..=100).map(f64::from).collect();
        let s = summarize(&v).unwrap();
        assert_eq!(s.n, 100);
        assert!((s.p50 - 50.5).abs() < 1e-9);
        assert!((s.p99 - 99.01).abs() < 1e-9);
        assert!((s.max - 100.0).abs() < f64::EPSILON);
    }
}

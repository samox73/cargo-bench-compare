use serde::Serialize;

#[derive(Clone, Serialize)]
pub struct Summary {
    pub n: usize,
    pub mean: f64,
    pub std_dev: f64,
    pub min: f64,
    pub max: f64,
}

pub fn summarize(values: &[f64]) -> Summary {
    let n = values.len();
    let mean = values.iter().sum::<f64>() / n as f64;
    let std_dev = if n < 2 {
        0.0
    } else {
        let variance = values
            .iter()
            .map(|v| {
                let d = v - mean;
                d * d
            })
            .sum::<f64>()
            / (n - 1) as f64;
        variance.sqrt()
    };
    let min = values.iter().copied().fold(f64::INFINITY, f64::min);
    let max = values.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    Summary {
        n,
        mean,
        std_dev,
        min,
        max,
    }
}

#[derive(Clone, Serialize)]
pub struct Comparison {
    pub id: String,
    pub unit: String,
    pub lower_is_better: bool,
    pub base: Summary,
    pub candidate: Summary,
    pub rel_diff_pct: f64,
    pub significant: bool,
    pub verdict: Verdict,
}

#[derive(Clone, Serialize, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Improved,
    Regressed,
    NoChange,
}

pub fn compare(
    id: String,
    unit: String,
    lower_is_better: bool,
    base: Summary,
    candidate: Summary,
) -> Comparison {
    let delta = candidate.mean - base.mean;
    let rel_diff_pct = if base.mean == 0.0 {
        f64::NAN
    } else {
        delta / base.mean * 100.0
    };
    let threshold = (base.std_dev.powi(2) + candidate.std_dev.powi(2)).sqrt();
    let significant = delta.abs() > threshold;
    let verdict = if !significant {
        Verdict::NoChange
    } else if (lower_is_better && delta < 0.0) || (!lower_is_better && delta > 0.0) {
        Verdict::Improved
    } else {
        Verdict::Regressed
    };
    Comparison {
        id,
        unit,
        lower_is_better,
        base,
        candidate,
        rel_diff_pct,
        significant,
        verdict,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_sample_std_dev() {
        let s = summarize(&[1.0, 2.0, 3.0, 4.0, 5.0]);
        assert_eq!(s.mean, 3.0);
        assert!((s.std_dev - 1.581_138_8).abs() < 1e-6);
        assert_eq!(s.min, 1.0);
        assert_eq!(s.max, 5.0);
    }

    #[test]
    fn summary_one_value() {
        assert_eq!(summarize(&[7.0]).std_dev, 0.0);
    }

    #[test]
    fn significance_threshold_flips() {
        let base = Summary {
            n: 1,
            mean: 10.0,
            std_dev: 1.0,
            min: 10.0,
            max: 10.0,
        };
        let below = Summary {
            n: 1,
            mean: 11.41,
            std_dev: 1.0,
            min: 11.41,
            max: 11.41,
        };
        let above = Summary {
            n: 1,
            mean: 11.42,
            std_dev: 1.0,
            min: 11.42,
            max: 11.42,
        };
        assert!(!compare("x".into(), "s".into(), true, base.clone(), below).significant);
        assert!(compare("x".into(), "s".into(), true, base, above).significant);
    }
}

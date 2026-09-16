//! Anomaly detection (architecture §6): rolling MAD, seasonal-adjusted (when a
//! model is available), and rate-of-change.

use chrono::Utc;

use crate::Result;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AnomalyMethod {
    Mad,
    Seasonal,
    RateOfChange,
}

impl AnomalyMethod {
    pub fn as_str(&self) -> &'static str {
        match self {
            AnomalyMethod::Mad => "mad",
            AnomalyMethod::Seasonal => "seasonal",
            AnomalyMethod::RateOfChange => "rate_of_change",
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct AnomalyResult {
    pub ts: chrono::DateTime<Utc>,
    pub metric: String,
    pub score: f64,
    pub method: AnomalyMethod,
    pub severity: String,
}

/// Run all three detectors against the trailing window `series` (newest last).
/// `metric` is the metric name to label the results with.
pub fn detect_anomalies_once(
    metric: &str,
    series: &[f64],
    timestamps: &[chrono::DateTime<Utc>],
    sensitivity_z: f64,
) -> Result<Vec<AnomalyResult>> {
    if series.len() < 5 || timestamps.len() != series.len() {
        return Ok(Vec::new());
    }

    let mut out = Vec::new();
    let z_threshold = sensitivity_z;

    // --- MAD over the trailing window, evaluated against the most recent point.
    if let Some(score) = mad_z(series) {
        if score.abs() >= z_threshold {
            let severity = severity_from_score(score.abs());
            out.push(AnomalyResult {
                ts: *timestamps.last().unwrap(),
                metric: metric.to_string(),
                score,
                method: AnomalyMethod::Mad,
                severity,
            });
        }
    }

    // --- Rate-of-change: did the latest step exceed the threshold relative to
    // its neighbors?
    if let Some(score) = rate_of_change_z(series) {
        if score.abs() >= z_threshold {
            let severity = severity_from_score(score.abs());
            out.push(AnomalyResult {
                ts: *timestamps.last().unwrap(),
                metric: metric.to_string(),
                score,
                method: AnomalyMethod::RateOfChange,
                severity,
            });
        }
    }

    // --- Seasonal-adjusted: simple seasonal subtraction. If series length >= 2*period
    // for a daily period, compare last point to one period ago.
    let day = 1440i64; // assume 1-minute rollups
    if series.len() as i64 >= 2 * day {
        let prev = series[series.len() - 1 - day as usize];
        let cur = *series.last().unwrap();
        let median = median(series).unwrap_or(0.0);
        let baseline = (prev - median).abs();
        if baseline > 1e-9 && (cur - prev).abs() / baseline > 3.0 {
            let score = (cur - prev) / baseline.max(1e-9);
            out.push(AnomalyResult {
                ts: *timestamps.last().unwrap(),
                metric: metric.to_string(),
                score,
                method: AnomalyMethod::Seasonal,
                severity: severity_from_score(score.abs()),
            });
        }
    }

    Ok(out)
}

/// Modified z-score using median absolute deviation.
/// Returns the z-score for the most recent point.
fn mad_z(series: &[f64]) -> Option<f64> {
    let n = series.len();
    if n < 5 {
        return None;
    }
    let med = median(series)?;
    let abs_devs: Vec<f64> = series.iter().map(|v| (v - med).abs()).collect();
    let mad = median(&abs_devs)?;
    if mad < 1e-9 {
        // Fall back to mean/stddev for low-variance series.
        let mean = series.iter().sum::<f64>() / n as f64;
        let var = series.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / n as f64;
        let std = var.sqrt();
        if std < 1e-9 {
            return None;
        }
        let last = *series.last()?;
        return Some((last - mean) / std);
    }
    let last = *series.last()?;
    // 0.6745 = 0.75th percentile of a normal distribution.
    Some(0.6745 * (last - med) / mad)
}

fn rate_of_change_z(series: &[f64]) -> Option<f64> {
    let n = series.len();
    if n < 5 {
        return None;
    }
    let diffs: Vec<f64> = series.windows(2).map(|w| w[1] - w[0]).collect();
    if diffs.is_empty() {
        return None;
    }
    let med = median(&diffs)?;
    let abs_devs: Vec<f64> = diffs.iter().map(|v| (v - med).abs()).collect();
    let mad = median(&abs_devs)?;
    let last = *diffs.last()?;
    if mad < 1e-9 {
        let mean = diffs.iter().sum::<f64>() / diffs.len() as f64;
        let var = diffs.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / diffs.len() as f64;
        let std = var.sqrt();
        if std < 1e-9 {
            return None;
        }
        return Some((last - mean) / std);
    }
    Some(0.6745 * (last - med) / mad)
}

fn median(values: &[f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut v: Vec<f64> = values.to_vec();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mid = v.len() / 2;
    if v.len() % 2 == 0 {
        Some((v[mid - 1] + v[mid]) / 2.0)
    } else {
        Some(v[mid])
    }
}

fn severity_from_score(s: f64) -> String {
    if s >= 6.0 {
        "critical".into()
    } else if s >= 4.0 {
        "high".into()
    } else if s >= 3.0 {
        "medium".into()
    } else {
        "low".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_spike() {
        let now = Utc::now();
        let mut series: Vec<f64> = (0..60).map(|_| 10.0).collect();
        series.push(500.0);
        let ts: Vec<_> = (0..61).map(|i| now - chrono::Duration::seconds((60 - i) as i64)).collect();
        let anomalies = detect_anomalies_once("vol", &series, &ts, 3.0).unwrap();
        assert!(anomalies.iter().any(|a| a.method == AnomalyMethod::Mad));
    }
}

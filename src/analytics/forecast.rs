//! Time-series forecasting via `augurs` (architecture §5).
//!
//! Model selection:
//! - **MSTL + ETS trend** when the series covers at least two full days —
//!   decomposes daily (and weekly, when there's enough history) seasonality
//!   with STL, then forecasts the deseasonalised trend with non-seasonal
//!   ETS. This is the same decomposition idea Prophet uses, and unlike
//!   plain ETS it projects the *shape* of the day/week instead of a flat
//!   mean line.
//! - **Non-seasonal ETS ("ZZN")** otherwise. NOTE: augurs-ets 0.10 has an
//!   unimplemented seasonal component (`todo!()` panic) — seasonal specs
//!   must never be passed to AutoETS directly.
//!
//! The HTTP handler wraps calls in `catch_unwind` as defense in depth.

use std::borrow::Cow;

use augurs_core::{Fit, Forecast, Predict};
use augurs_mstl::{FittedTrendModel, MSTLModel, TrendModel};
use chrono::Utc;

use crate::Result;

/// One forecast point — timestamp + predicted value with a confidence interval.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ForecastPoint {
    pub ts: chrono::DateTime<Utc>,
    pub predicted: f64,
    pub lower: f64,
    pub upper: f64,
}

#[derive(Debug, Clone, serde::Deserialize, schemars::JsonSchema)]
pub struct ForecastRequest {
    /// Past observations, oldest first. Length must be >= 4.
    pub series: Vec<f64>,
    /// Sampling interval of the series, in seconds. e.g. 60 for 1-minute rollups.
    pub interval_secs: i64,
    /// Number of points to forecast.
    pub horizon: usize,
    /// Confidence interval level (0..1). Default 0.95.
    #[serde(default = "default_level")]
    pub level: f64,
    /// First forecast timestamp. Defaults to "now" (sensible for live
    /// rollups); the dashboard passes the last history point so overlays
    /// line up even for historical custom ranges.
    #[serde(default)]
    pub start: Option<chrono::DateTime<Utc>>,
}

fn default_level() -> f64 {
    0.95
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ForecastResponse {
    pub points: Vec<ForecastPoint>,
    pub model: String,
    pub horizon: usize,
}

// =====================================================================
// ETS-backed trend model for MSTL
// =====================================================================

/// Non-seasonal AutoETS wrapped as an MSTL [`TrendModel`].
#[derive(Debug)]
struct EtsTrend;

impl TrendModel for EtsTrend {
    fn name(&self) -> Cow<'_, str> {
        "ets".into()
    }
    fn fit(
        &self,
        y: &[f64],
    ) -> std::result::Result<
        Box<dyn FittedTrendModel + Sync + Send>,
        Box<dyn std::error::Error + Send + Sync + 'static>,
    > {
        // "ZZN": auto error + trend, NO seasonality (the seasonal component
        // is a `todo!()` panic in augurs-ets 0.10 — never request it here).
        let forecaster = augurs_ets::AutoETS::new(1, "ZZN")
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;
        let fit = forecaster
            .fit(y)
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;
        let n = y.len();
        Ok(Box::new(EtsTrendFitted { fit, n }))
    }
}

#[derive(Debug)]
struct EtsTrendFitted {
    fit: <augurs_ets::AutoETS as Fit>::Fitted,
    n: usize,
}

impl FittedTrendModel for EtsTrendFitted {
    fn training_data_size(&self) -> Option<usize> {
        Some(self.n)
    }
    fn predict_inplace(
        &self,
        horizon: usize,
        level: Option<f64>,
        forecast: &mut Forecast,
    ) -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync + 'static>> {
        let f = self
            .fit
            .predict(horizon, level)
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;
        *forecast = f;
        Ok(())
    }
    fn predict_in_sample_inplace(
        &self,
        level: Option<f64>,
        forecast: &mut Forecast,
    ) -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync + 'static>> {
        let f = self
            .fit
            .predict_in_sample(level)
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;
        *forecast = f;
        Ok(())
    }
}

/// Seasonal periods (in samples) usable by MSTL for this series, from the
/// sampling interval. A period only qualifies if the series covers at least
/// two full cycles (STL needs that much to separate the component).
fn seasonal_periods(len: usize, interval_secs: i64) -> Vec<usize> {
    let mut periods = Vec::new();
    let daily = ((24 * 3600) / interval_secs.max(1)) as usize;
    let weekly = daily.saturating_mul(7);
    if daily >= 2 && len >= daily * 2 {
        periods.push(daily);
        if weekly >= 2 && len >= weekly * 2 {
            periods.push(weekly);
        }
    }
    periods
}

/// Run the forecast: MSTL + ETS when seasonality is detectable, plain
/// non-seasonal ETS otherwise.
pub fn forecast_series(req: &ForecastRequest) -> Result<ForecastResponse> {
    if req.series.len() < 4 {
        return Err(crate::Error::invalid_input(
            "need at least 4 observations to forecast",
        ));
    }
    if req.horizon == 0 {
        return Err(crate::Error::invalid_input("horizon must be > 0"));
    }
    if !(0.0..=1.0).contains(&req.level) {
        return Err(crate::Error::invalid_input("level must be in [0, 1]"));
    }

    let y: Vec<f64> = req.series.iter().cloned().collect();
    let periods = seasonal_periods(y.len(), req.interval_secs);

    let (predicted, lower, upper, model) = if periods.is_empty() {
        let forecaster = augurs_ets::AutoETS::new(1, "ZZN").map_err(|e| {
            crate::Error::invalid_input(format!("augurs ets construct failed: {e:?}"))
        })?;
        let fit = forecaster
            .fit(&y)
            .map_err(|e| crate::Error::invalid_input(format!("augurs ets fit failed: {e:?}")))?;
        let forecast = fit.predict(req.horizon, req.level).map_err(|e| {
            crate::Error::invalid_input(format!("augurs ets predict failed: {e:?}"))
        })?;
        let (l, u) = split_intervals(&forecast);
        (forecast.point, l, u, "ets(non-seasonal)".to_string())
    } else {
        let model = MSTLModel::new(periods.clone(), EtsTrend);
        let fit = model
            .fit(&y)
            .map_err(|e| crate::Error::invalid_input(format!("mstl fit failed: {e:?}")))?;
        let forecast = fit
            .predict(req.horizon, req.level)
            .map_err(|e| crate::Error::invalid_input(format!("mstl predict failed: {e:?}")))?;
        let (l, u) = split_intervals(&forecast);
        let seasons = periods
            .iter()
            .map(|p| p.to_string())
            .collect::<Vec<_>>()
            .join(",");
        (forecast.point, l, u, format!("mstl-ets(s=[{seasons}])"))
    };

    let start_ts = req.start.unwrap_or_else(Utc::now);
    let interval_dur = chrono::Duration::seconds(req.interval_secs.max(1));
    let points = predicted
        .iter()
        .zip(lower.iter().zip(upper.iter()))
        .enumerate()
        .map(|(i, (p, (l, u)))| ForecastPoint {
            ts: start_ts + interval_dur * ((i + 1) as i32),
            predicted: *p,
            lower: *l,
            upper: *u,
        })
        .collect();

    Ok(ForecastResponse {
        points,
        model,
        horizon: req.horizon,
    })
}

fn split_intervals(forecast: &Forecast) -> (Vec<f64>, Vec<f64>) {
    match &forecast.intervals {
        Some(i) => (i.lower.clone(), i.upper.clone()),
        None => (forecast.point.clone(), forecast.point.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_short_series_and_bad_params() {
        let mut req = ForecastRequest {
            series: vec![1.0, 2.0],
            interval_secs: 60,
            horizon: 5,
            level: 0.95,
            start: None,
        };
        assert!(forecast_series(&req).is_err());
        req.series = vec![1.0, 2.0, 3.0, 4.0, 5.0];
        req.horizon = 0;
        assert!(forecast_series(&req).is_err());
        req.horizon = 3;
        req.level = 1.5;
        assert!(forecast_series(&req).is_err());
    }

    #[test]
    fn seasonality_gates() {
        // 1-minute samples: daily = 1440, weekly = 10080.
        assert!(seasonal_periods(1000, 60).is_empty(), "under 2 days");
        assert_eq!(
            seasonal_periods(1440 * 2, 60),
            vec![1440],
            "2 days: daily only"
        );
        assert_eq!(
            seasonal_periods(10080 * 2, 60),
            vec![1440, 10080],
            "2 weeks: daily + weekly"
        );
        // Hourly samples: daily = 24.
        assert_eq!(seasonal_periods(24 * 2, 3600), vec![24]);
        // Interval >= a day: no daily seasonality expressible.
        assert!(seasonal_periods(100, 86400).is_empty());
    }

    #[test]
    fn non_seasonal_forecast_runs() {
        let req = ForecastRequest {
            series: vec![1.0, 3.0, 2.0, 4.0, 3.0, 5.0, 4.0, 6.0],
            interval_secs: 60,
            horizon: 4,
            level: 0.95,
            start: Some(
                chrono::DateTime::parse_from_rfc3339("2026-09-15T00:00:00Z")
                    .unwrap()
                    .with_timezone(&Utc),
            ),
        };
        let resp = forecast_series(&req).unwrap();
        assert_eq!(resp.model, "ets(non-seasonal)");
        assert_eq!(resp.points.len(), 4);
        // Anchored at the requested start + interval steps.
        assert_eq!(resp.points[0].ts.to_rfc3339(), "2026-09-15T00:01:00+00:00");
        assert_eq!(resp.points[3].ts.to_rfc3339(), "2026-09-15T00:04:00+00:00");
        for p in &resp.points {
            assert!(p.lower <= p.upper);
        }
    }

    #[test]
    fn mstl_forecast_tracks_daily_shape() {
        // Two days of 15-minute samples with a clear day/night cycle.
        let per_day = 96;
        let series: Vec<f64> = (0..per_day * 2)
            .map(|i| {
                let hour = (i % per_day) as f64 / 4.0;
                10.0 + 8.0 * (-((hour - 14.0) / 6.0).powi(2)).exp() + (i as f64) * 0.01
            })
            .collect();
        let req = ForecastRequest {
            series,
            interval_secs: 900,
            horizon: per_day, // project a full day ahead
            level: 0.95,
            start: None,
        };
        let resp = forecast_series(&req).unwrap();
        assert!(resp.model.starts_with("mstl-ets"), "model: {}", resp.model);
        assert_eq!(resp.points.len(), per_day);
        // A flat ETS would produce near-constant predictions; MSTL should
        // reproduce intraday variance comparable to the input's.
        let preds: Vec<f64> = resp.points.iter().map(|p| p.predicted).collect();
        let span = preds.iter().cloned().fold(f64::MIN, f64::max)
            - preds.iter().cloned().fold(f64::MAX, f64::min);
        let input_span = 8.0; // the gaussian bump height
        assert!(
            span > input_span * 0.5,
            "forecast should track the daily shape, span was {span}"
        );
    }
}

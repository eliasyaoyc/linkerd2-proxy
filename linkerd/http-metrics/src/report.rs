use super::{ClassMetrics, Registry, RequestMetrics, StatusMetrics};
use http;
use linkerd2_metrics::{latency, Counter, FmtLabels, FmtMetric, FmtMetrics, Histogram, Metric};
use std::fmt;
use std::hash::Hash;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_timer::clock;
use tracing::trace;

impl<T, C> Report<T, Metrics<C>>
where
    T: FmtLabels + Hash + Eq,
    C: FmtLabels + Hash + Eq,
{
    fn request_total(&self) -> Metric<'_, Prefixed<'_, &'static str>, Counter> {
        Metric::new(
            self.prefix_key("request_total"),
            "Total count of HTTP requests.",
        )
    }

    fn response_total(&self) -> Metric<'_, Prefixed<'_, &'static str>, Counter> {
        Metric::new(
            self.prefix_key("response_total"),
            "Total count of HTTP responses.",
        )
    }

    fn response_latency_ms(
        &self,
    ) -> Metric<'_, Prefixed<'_, &'static str>, Histogram<latency::Ms>> {
        Metric::new(
            self.prefix_key("response_latency_ms"),
            "Elapsed times between a request's headers being received \
             and its response stream completing",
        )
    }
}

impl<T, C> FmtMetrics for Report<T, Metrics<C>>
where
    T: FmtLabels + Hash + Eq,
    C: FmtLabels + Hash + Eq,
{
    fn fmt_metrics(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut registry = match self.registry.lock() {
            Err(_) => return Ok(()),
            Ok(r) => r,
        };

        let now = clock::now();
        let since = now - self.retain_idle;
        registry.retain_since(since);
        trace!(
            prfefix = %self.prefix,
            ?now,
            ?since,
            targets = %registry.by_target.len(),
            "Formatting HTTP request metrics",
        );
        let registry = registry; // Mutability no longer needed.

        if registry.by_target.is_empty() {
            return Ok(());
        }

        self.request_total().fmt_help(f)?;
        registry.fmt_by_target(f, self.request_total(), |s| &s.total)?;

        self.response_latency_ms().fmt_help(f)?;
        registry.fmt_by_status(f, self.response_latency_ms(), |s| &s.latency)?;

        self.response_total().fmt_help(f)?;
        registry.fmt_by_class(f, self.response_total(), |s| &s.total)?;

        Ok(())
    }
}

impl<T, C> Registry<T, Metrics<C>>
where
    T: FmtLabels + Hash + Eq,
    C: FmtLabels + Hash + Eq,
{
    fn fmt_by_target<N, V, F>(
        &self,
        f: &mut fmt::Formatter<'_>,
        metric: Metric<'_, N, V>,
        get_metric: F,
    ) -> fmt::Result
    where
        N: fmt::Display,
        V: FmtMetric,
        F: Fn(&Metrics<C>) -> &V,
    {
        for (tgt, tm) in &self.by_target {
            if let Ok(m) = tm.lock() {
                get_metric(&*m).fmt_metric_labeled(f, &metric.name, tgt)?;
            }
        }

        Ok(())
    }

    fn fmt_by_status<N, M, F>(
        &self,
        f: &mut fmt::Formatter<'_>,
        metric: Metric<'_, N, M>,
        get_metric: F,
    ) -> fmt::Result
    where
        N: fmt::Display,
        M: FmtMetric,
        F: Fn(&StatusMetrics<C>) -> &M,
    {
        for (tgt, tm) in &self.by_target {
            if let Ok(tm) = tm.lock() {
                for (status, m) in &tm.by_status {
                    let status = status.as_ref().map(|s| Status(*s));
                    let labels = (tgt, status);
                    get_metric(&*m).fmt_metric_labeled(f, &metric.name, labels)?;
                }
            }
        }

        Ok(())
    }

    fn fmt_by_class<N, M, F>(
        &self,
        f: &mut fmt::Formatter<'_>,
        metric: Metric<'_, N, M>,
        get_metric: F,
    ) -> fmt::Result
    where
        N: fmt::Display,
        M: FmtMetric,
        F: Fn(&ClassMetrics) -> &M,
    {
        for (tgt, tm) in &self.by_target {
            if let Ok(tm) = tm.lock() {
                for (status, sm) in &tm.by_status {
                    for (cls, m) in &sm.by_class {
                        let status = status.as_ref().map(|s| Status(*s));
                        let labels = (tgt, (status, cls));
                        get_metric(&*m).fmt_metric_labeled(f, &metric.name, labels)?;
                    }
                }
            }
        }

        Ok(())
    }
}

impl FmtLabels for Status {
    fn fmt_labels(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "status_code=\"{}\"", self.0.as_u16())
    }
}

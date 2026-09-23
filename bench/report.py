import statistics
from .profiles import differences


METRICS = {"wall_seconds": ("wall_seconds",),
           "peak_target_rss_bytes": ("target", "rss_bytes"),
           "observed_target_cpu_seconds": ("target", "observed_cpu_seconds"),
           "observed_turn_p99_ms": ("events", "observed_turn_ms", "p99"),
           "observed_first_chunk_p99_ms": ("events", "observed_first_chunk_ms", "p99"),
           "request_body_bytes": ("provider", "request_body_bytes"),
           "response_body_bytes": ("provider", "response_body_bytes"),
           "connections_used": ("provider", "connections_used")}


# Model fixtures that validate the same synthetic conversation semantics.
MODEL_PROTOCOLS = {'responses', 'gateway', 'anthropic_messages'}
EXPLORATORY_ONLY = ('provider_protocol', 'rss_limit_mib', 'process_limit')


def comparison_runs(result):
    runs = [run for run in result["runs"] if not run.get("warmup")]
    if not runs or any(run["status"] != "ok" for run in result["runs"]):
        raise ValueError("cannot compare failed or empty benchmark runs")
    return runs


def values(runs, path):
    output = []
    for run in runs:
        value = run
        for key in path:
            value = value[key]
        if not isinstance(value, (int, float)):
            raise ValueError("metric is unavailable")
        output.append(value)
    return output


def compare(base, candidate, *, exploratory=False):
    if base.get("schema") != 1 or candidate.get("schema") != 1:
        raise ValueError("unsupported result schema")
    compatibility_gaps = []
    if base["compatibility"] != candidate["compatibility"]:
        a, b = dict(base['compatibility']), dict(candidate['compatibility'])
        differing = {key for key in EXPLORATORY_ONLY if a.pop(key, None) != b.pop(key, None)}
        protocols = {base['compatibility'].get('provider_protocol'),
                     candidate['compatibility'].get('provider_protocol')}
        # Same semantic fixture, different native wire protocols or per-engine
        # sampled guards. This never relaxes host/workload/observer matching
        # or permits a ranking.
        if not exploratory or a != b or not protocols <= MODEL_PROTOCOLS:
            raise ValueError("workload, host, observer, or sampling settings differ")
        if 'provider_protocol' in differing:
            compatibility_gaps.append(
                'provider_protocol differs: ' + ' versus '.join(sorted(protocols))
                + '; serialization, side requests, and wire bytes are not equivalent')
        if differing - {'provider_protocol'}:
            compatibility_gaps.append(
                'sampled guard limits differ (' + ', '.join(sorted(differing - {'provider_protocol'}))
                + '); they bound runaway targets and failed runs are never compared')
    left, right = comparison_runs(base), comparison_runs(candidate)
    gaps = differences(base, candidate) + compatibility_gaps
    if gaps and not exploratory:
        raise ValueError('feature profiles differ or are unknown; use --exploratory for unranked observations')
    for result, runs in ((base, left), (candidate, right)):
        configured = result.get('workload', {}).get('concurrency')
        if configured is None or any(run.get('provider', {}).get('peak_active_requests') != configured for run in runs):
            raise ValueError('configured provider concurrency was not achieved or is unknown')
    report = {"baseline_runs": len(left), "candidate_runs": len(right), "metrics": {}}
    report['classification'] = 'exploratory_unmatched_footprints' if gaps else 'matched_configuration_regression'
    report['feature_gaps'] = gaps
    report['profiles'] = {side: result.get('target_metadata', {}).get('comparison_profile')
                          for side, result in [('baseline', base), ('candidate', candidate)]}
    report["achieved_concurrency"] = {
        "baseline": [run.get("provider", {}).get("peak_active_requests") for run in left],
        "candidate": [run.get("provider", {}).get("peak_active_requests") for run in right]}
    report["quality_warnings"] = sorted({warning for run in [*left, *right]
                                         for warning in run.get("quality_warnings", [])})
    for name, path in METRICS.items():
        a, b = values(left, path), values(right, path)
        old, new = statistics.median(a), statistics.median(b)
        report["metrics"][name] = {
            "baseline_median": old, "candidate_median": new,
            "baseline_range": [min(a), max(a)], "candidate_range": [min(b), max(b)]}
        if not gaps:
            report['metrics'][name]['change_percent'] = (new / old - 1) * 100 if old else None
    report["note"] = ('Descriptive observations, not feature-equivalent harness efficiency or feature cost attribution. '
                      'Unmatched results omit percentage rankings. Ranges are not confidence intervals.' if gaps else
                      'Same declared configuration regression only, not full product parity. Ranges are not confidence intervals.')
    return report

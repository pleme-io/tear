.read tear-bench/sql/views.sql

create or replace view latency as
select run, bench, variant, name, "case", metric,
       count(*) filter (where isfinite(value)) as n,
       count(*) filter (where not isfinite(value)) as lost,
       round(quantile_cont(value, 0.5) filter (where isfinite(value)) / 1000, 2) as p50_us,
       round(quantile_cont(value, 0.9) filter (where isfinite(value)) / 1000, 2) as p90_us,
       round(quantile_cont(value, 0.99) filter (where isfinite(value)) / 1000, 2) as p99_us,
       round(max(value) filter (where isfinite(value)) / 1000, 2) as max_us
from samples
where unit = 'ns'
group by all
order by run, bench, variant, name;

create or replace view counts as
select run, bench, variant, name, "case", metric, unit,
       count(*) as n,
       min(value) as min, max(value) as max, sum(value) as total,
       bool_and(ok) as all_ok
from samples
where unit <> 'ns'
group by all
order by run, bench, variant, name;

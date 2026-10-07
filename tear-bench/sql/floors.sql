.read tear-bench/sql/views.sql

create or replace view floors as
select run, replace(bench, 'floor:', '') as floor, variant,
       count(*) as n,
       round(quantile_cont(value, 0.5) / 1000, 3) as p50_us,
       round(quantile_cont(value, 0.9) / 1000, 3) as p90_us,
       round(quantile_cont(value, 0.99) / 1000, 3) as p99_us,
       round(max(value) / 1000, 3) as max_us
from samples
where bench like 'floor:%' and isfinite(value)
group by all
order by run, floor, variant;

.read tear-bench/sql/views.sql

create or replace view tally as
select run, verdict, count(*) as cells
from verdicts
group by all
order by run, verdict;

create or replace view graded as
select run, "case", metric, budget, verdict, n, value, "limit", detail
from verdicts
where verdict not in ('not-applicable')
order by run, "case", metric;

create or replace view blind_streak as
with classed as (
    select run,
           max(value) filter (where key = 'host-class') as host_class,
           bool_or(key = 'sentinels' and value like 'blind%') as blind
    from runs
    group by run
),
ordered as (
    select *, row_number() over (partition by host_class order by run desc) as age,
           sum(case when blind then 0 else 1 end) over (partition by host_class order by run desc
                                                        rows between unbounded preceding and current row) as quiet_since
    from classed
    where host_class is not null
)
select host_class, count(*) filter (where quiet_since = 0) as trailing_blind_runs,
       count(*) filter (where quiet_since = 0) >= 3 as red
from ordered
group by host_class;

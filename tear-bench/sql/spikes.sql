.read tear-bench/sql/views.sql

create or replace view spikes as
select run, variant, i, value / 1e6 as echo_ms,
       cast(regexp_extract(detail, 't_send_ms=([0-9.]+)', 1) as double) as t_send_ms
from samples
where bench = 'series' and name = 'echo' and isfinite(value) and value > 3e6;

create or replace view spike_gaps as
select run, variant, t_send_ms,
       t_send_ms - lag(t_send_ms) over (partition by run, variant order by t_send_ms) as gap_ms
from spikes;

create or replace view one_hertz as
select run, variant,
       (select count(*) from spikes s where s.run = g.run and s.variant = g.variant) as spikes,
       count(gap_ms) as gaps,
       count(*) filter (where gap_ms between 990 and 1040) as gaps_at_one_hertz
from spike_gaps g
group by run, variant
order by run, variant;

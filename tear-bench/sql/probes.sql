.read tear-bench/sql/views.sql

create or replace view probe_totals as
select run, split_part(role, ':', 1) as variant, split_part(role, ':', 2) as role,
       kind, name, count(distinct pid) as processes, sum(value) as total, max(value) as worst
from probes
group by all
order by run, variant, role, kind, name;

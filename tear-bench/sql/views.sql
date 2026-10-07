create or replace view samples as
select *
from read_csv(getvariable('data') || '/samples.tsv', delim = '\t', header = true, quote = '',
              columns = {'run': 'VARCHAR', 'bench': 'VARCHAR', 'variant': 'VARCHAR', 'case': 'VARCHAR',
                         'metric': 'VARCHAR', 'i': 'BIGINT', 'name': 'VARCHAR', 'value': 'DOUBLE',
                         'unit': 'VARCHAR', 'ok': 'BOOLEAN', 'detail': 'VARCHAR'});

create or replace view verdicts as
select *
from read_csv(getvariable('data') || '/verdicts.tsv', delim = '\t', header = true, quote = '',
              columns = {'run': 'VARCHAR', 'case': 'VARCHAR', 'metric': 'VARCHAR', 'budget': 'VARCHAR',
                         'verdict': 'VARCHAR', 'n': 'BIGINT', 'value': 'DOUBLE', 'limit': 'DOUBLE',
                         'detail': 'VARCHAR'});

create or replace view runs as
select *
from read_csv(getvariable('data') || '/runs.tsv', delim = '\t', header = true, quote = '',
              columns = {'run': 'VARCHAR', 'key': 'VARCHAR', 'value': 'VARCHAR'});

create or replace view probes as
select *
from read_csv(getvariable('data') || '/probes.tsv', delim = '\t', header = true, quote = '',
              columns = {'run': 'VARCHAR', 'role': 'VARCHAR', 'pid': 'VARCHAR', 'kind': 'VARCHAR',
                         'name': 'VARCHAR', 'value': 'UBIGINT'});

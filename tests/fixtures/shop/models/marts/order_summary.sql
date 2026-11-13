-- Its upstream is a variable: only dbt knows it.
select count(*) as orders from {{ ref(var('summary_upstream')) }}

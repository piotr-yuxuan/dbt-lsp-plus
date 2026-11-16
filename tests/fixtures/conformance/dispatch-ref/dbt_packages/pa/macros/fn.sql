{% macro fn() %}{{ return(adapter.dispatch('fn', 'pa')()) }}{% endmacro %}
{% macro default__fn() %}select * from {{ ref('x') }}{% endmacro %}
{% macro duckdb__fn() %}select * from {{ ref('y') }}{% endmacro %}

{% macro fn() %}{{ return(adapter.dispatch('fn', 'pa')()) }}{% endmacro %}
{% macro default__fn() %}1{% endmacro %}
{% macro duckdb__fn() %}2{% endmacro %}
{% macro postgres__fn() %}3{% endmacro %}

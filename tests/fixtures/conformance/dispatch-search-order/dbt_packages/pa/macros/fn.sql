{% macro fn() %}{{ return(adapter.dispatch('fn', 'pa')()) }}{% endmacro %}
{% macro default__fn() %}1{% endmacro %}

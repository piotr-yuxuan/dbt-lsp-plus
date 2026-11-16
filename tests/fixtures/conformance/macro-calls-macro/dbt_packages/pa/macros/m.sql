{% macro outer() %}{{ inner() }}{% endmacro %}
{% macro inner() %}2{% endmacro %}

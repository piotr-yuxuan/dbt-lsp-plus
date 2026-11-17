{% macro outer() %}{{ inner() }}{% endmacro %}
{% macro inner() %}'pa'{% endmacro %}

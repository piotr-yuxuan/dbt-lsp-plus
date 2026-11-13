{% macro is_positive(column) -%}
    ({{ column }} > 0)
{%- endmacro %}

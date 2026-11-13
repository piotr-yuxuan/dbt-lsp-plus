{% macro cents_to_dollars(column_name, scale=2) -%}
    ({{ column_name }} / 100)::numeric(16, {{ scale }})
{%- endmacro %}

{% macro dollars_to_cents(column_name) -%}
    ({{ column_name }} * 100)::bigint
{%- endmacro %}

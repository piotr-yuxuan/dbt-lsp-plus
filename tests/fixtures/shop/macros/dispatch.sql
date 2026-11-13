{% macro safe_add(a, b) -%}
    {{ return(adapter.dispatch('safe_add', 'shop')(a, b)) }}
{%- endmacro %}

{% macro default__safe_add(a, b) -%}
    ({{ a }} + {{ b }})
{%- endmacro %}

{% macro duckdb__safe_add(a, b) -%}
    (coalesce({{ a }}, 0) + coalesce({{ b }}, 0))
{%- endmacro %}

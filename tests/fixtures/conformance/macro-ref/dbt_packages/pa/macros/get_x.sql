{% macro get_x() %}select * from {{ ref('x') }}{% endmacro %}

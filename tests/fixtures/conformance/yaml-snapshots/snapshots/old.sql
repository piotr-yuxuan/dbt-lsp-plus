{% snapshot old %}
{{ config(unique_key='id', strategy='check', check_cols='all') }}
select * from {{ ref('x') }}
{% endsnapshot %}

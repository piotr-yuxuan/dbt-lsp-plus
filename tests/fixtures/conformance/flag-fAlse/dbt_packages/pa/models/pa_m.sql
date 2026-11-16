select * from {{ ref('x') }}
union all
select * from {{ source('s', 't') }}

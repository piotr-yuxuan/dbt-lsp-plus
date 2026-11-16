select {{ function('double_it') }}(id) as d from {{ ref('x') }}

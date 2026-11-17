select * from {{ ref('m') }}, {{ ref('m', v=1) }}, {{ ref('n') }}, {{ ref('n', version=2) }},
  {{ ref('k') }}, {{ ref('k', v=1) }}, {{ ref('p') }}

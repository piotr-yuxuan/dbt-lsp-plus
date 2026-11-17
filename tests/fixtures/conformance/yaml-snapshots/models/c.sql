select * from {{ ref('in_models') }}, {{ ref('in_snapshots') }}, {{ ref('old') }}, {{ ref('in_analyses') }}

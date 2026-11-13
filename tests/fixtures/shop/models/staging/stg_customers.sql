-- The customers, from the source rather than the seed's ref('raw_customers').
select
    id as customer_id,
    first_name,
    last_name
from {{ source('raw', 'customers') }}

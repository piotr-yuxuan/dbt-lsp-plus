select * from {{ source('stripe', 'charge') }}

with customer_orders as (
    select customer_id, count(*) as order_count
    from {{ ref('orders') }}
    group by customer_id
)

select stg.*, customer_orders.order_count
from {{ ref('stg_customers') }} as stg
left join customer_orders using (customer_id)

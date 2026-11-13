{#
  Orders with their customer.  It's a mart: one row per order.
#}
with orders as (
    select * from {{ ref('stg_orders') }}
),

customers as (
    select * from {{ ref('stg_customers') }}
),

final as (
    select
        orders.order_id,
        customers.first_name,
        orders.amount,
        {{ shop_utils.is_positive('orders.amount') }} as is_paid
    from orders
    left join customers using (customer_id)
)

select * from final

select status, count(*) from {{ ref('orders') }} group by status

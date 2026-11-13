select * from {{ source('events', 'page_views') }}

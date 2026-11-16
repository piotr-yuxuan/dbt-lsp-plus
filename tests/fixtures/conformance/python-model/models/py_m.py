def model(dbt, session):
    dbt.config(materialized='table')
    a = dbt.ref('x')
    b = dbt.source('s', 't')
    return a

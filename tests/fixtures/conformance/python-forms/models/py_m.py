def model(dbt, session):
    a = dbt.ref('pa', 'y')
    b = dbt.ref('m', v=1)
    c = dbt.ref('m', version=2)
    d = dbt.source('s', 't')
    e = dbt.ref('x')
    return a

---
title: Analyzing OnTime With Databend on Object Storage
sidebar_label: Analyzing OnTime Dataset
description: Use Databend analyzing `OnTime` datasets on S3 step by step.
---

Analyzing `OnTime` datasets on S3 with Databend step by step.

## Step 1. Deploy Databend

Make sure you have installed Databend, if not please see:

* [How to Deploy Databend](../00-guides/index.md#deployment)

## Step 2. Load OnTime Datasets

### 2.1 Create a Databend User

Connect to Databend server with MySQL client:
```shell
mysql -h127.0.0.1 -uroot -P3307 
```

Create a user:
```sql
CREATE USER user1 IDENTIFIED BY 'abc123';
```

Grant privileges for the user:
```sql
GRANT ALL ON *.* TO user1;
```

See also [How To Create User](../30-reference/30-sql/00-ddl/30-user/01-user-create-user.md).

### 2.2 Create OnTime Table

[Create SQL](https://github.com/datafuselabs/databend/blob/main/tests/suites/1_stateful/ddl/ontime.sql)

### 2.3 Load Data Into OnTime Table

```shell title='t_ontime.csv.zip'
wget --no-check-certificate https://repo.databend.rs/t_ontime/t_ontime.csv.zip
```

```shell title='Unzip'
unzip t_ontime.csv.zip
```

```shell title='Load CSV files into Databend'
ls *.csv|xargs -I{} echo  curl -H \"insert_sql:insert into ontime format CSV\" -H \"skip_header:0\" -H \"field_delimiter:\t\"  -F  \"upload=@{}\"  -XPUT http://user1:abc123@127.0.0.1:8081/v1/streaming_load |bash
```

:::tip

* http://user1:abc123@127.0.0.1:8081/v1/streaming_load
    * `user1` is the user.
    * `abc123` is the user password.
    * `127.0.0.1` is `http_handler_host` value in your *databend-query.toml*
    * `8081` is `http_handler_port` value in your *databend-query.toml*
:::

## Step 3. Queries

Execute Queries:

```shell
mysql -h127.0.0.1 -P3307 -uroot
```
```sql
SELECT Year, count(*) FROM ontime GROUP BY Year;
```

All Queries:

| Number      | Query |
| ----------- | ----------- |
| Q1   |SELECT DayOfWeek, count(*) AS c FROM ontime WHERE Year >= 2000 AND Year <= 2008 GROUP BY DayOfWeek ORDER BY c DESC;       |
| Q2   |SELECT DayOfWeek, count(*) AS c FROM ontime WHERE DepDelay>10 AND Year >= 2000 AND Year <= 2008 GROUP BY DayOfWeek ORDER BY c DESC;    |
| Q3   |SELECT Origin, count(*) AS c FROM ontime WHERE DepDelay>10 AND Year >= 2000 AND Year <= 2008 GROUP BY Origin ORDER BY c DESC LIMIT 10;   |
| Q4   |SELECT IATA_CODE_Reporting_Airline AS Carrier, count(*) FROM ontime WHERE DepDelay>10 AND Year = 2007 GROUP BY Carrier ORDER BY count(*) DESC;      |
| Q5   |SELECT IATA_CODE_Reporting_Airline AS Carrier, avg(cast(DepDelay>10 as Int8))*1000 AS c3 FROM ontime WHERE Year=2007 GROUP BY Carrier ORDER BY c3 DESC;|
| Q6   |SELECT IATA_CODE_Reporting_Airline AS Carrier, avg(cast(DepDelay>10 as Int8))*1000 AS c3 FROM ontime WHERE Year>=2000 AND Year <=2008 GROUP BY Carrier ORDER BY c3 DESC;|
| Q7   |SELECT IATA_CODE_Reporting_Airline AS Carrier, avg(DepDelay) * 1000 AS c3 FROM ontime WHERE Year >= 2000 AND Year <= 2008 GROUP BY Carrier; |
| Q8   |SELECT Year, avg(DepDelay) FROM ontime GROUP BY Year;      |
| Q9   |SELECT Year, count(*) as c1 FROM ontime GROUP BY Year;      |
| Q10  |SELECT avg(cnt) FROM (SELECT Year,Month,count(*) AS cnt FROM ontime WHERE DepDel15=1 GROUP BY Year,Month) a;      |
| Q11  |SELECT avg(c1) FROM (SELECT Year,Month,count(*) AS c1 FROM ontime GROUP BY Year,Month) a;      |
| Q12  |SELECT OriginCityName, DestCityName, count(*) AS c FROM ontime GROUP BY OriginCityName, DestCityName ORDER BY c DESC LIMIT 10;     |
| Q13  |SELECT OriginCityName, count(*) AS c FROM ontime GROUP BY OriginCityName ORDER BY c DESC LIMIT 10;      |
| Q14  |SELECT count(*) FROM ontime;     |


## Benchmark Report

* [Amazon S3: Databend Ontime Datasets Benchmark Report](../70-performance/01-ec2-s3-performance.md)
* [Tencent COS: Databend Ontime Datasets Benchmark Report](../70-performance/02-cvm-cos-performance.md)
* [Alibaba OSS: Databend Ontime Datasets Benchmark Report](../70-performance/03-ecs-oss-performance.md)
* [Wasabi: Databend Ontime Datasets Benchmark Report](../70-performance/04-ec2-wasabi-performance.md)

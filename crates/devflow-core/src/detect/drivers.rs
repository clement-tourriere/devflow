//! Database client libraries, by dependency name.
//!
//! These say which engine the application talks to even when no compose file
//! or `.env` exists to say so.
use super::facts::{DriverFact, Engine};
use super::manifest::{Deps, Discovery, Project};
use std::collections::HashSet;

/// Most driver facts a scan reports.
const MAX_DRIVERS: usize = 60;

const JS: &[(&str, Engine)] = &[
    ("pg", Engine::Postgres),
    ("pg-promise", Engine::Postgres),
    ("postgres", Engine::Postgres),
    ("slonik", Engine::Postgres),
    ("@prisma/adapter-pg", Engine::Postgres),
    ("@neondatabase/serverless", Engine::Postgres),
    ("@vercel/postgres", Engine::Postgres),
    ("mysql", Engine::Mysql),
    ("mysql2", Engine::Mysql),
    ("@planetscale/database", Engine::Mysql),
    ("mongodb", Engine::Mongodb),
    ("mongoose", Engine::Mongodb),
    ("redis", Engine::Redis),
    ("ioredis", Engine::Redis),
    ("bullmq", Engine::Redis),
    ("better-sqlite3", Engine::Sqlite),
    ("sqlite3", Engine::Sqlite),
    ("@libsql/client", Engine::Sqlite),
    ("@clickhouse/client", Engine::Clickhouse),
    ("mssql", Engine::SqlServer),
    ("tedious", Engine::SqlServer),
    ("memjs", Engine::Memcached),
    ("@elastic/elasticsearch", Engine::Elasticsearch),
    ("@opensearch-project/opensearch", Engine::Opensearch),
    ("meilisearch", Engine::Meilisearch),
    ("amqplib", Engine::Rabbitmq),
    ("kafkajs", Engine::Kafka),
];

const PYTHON: &[(&str, Engine)] = &[
    ("psycopg", Engine::Postgres),
    ("psycopg2", Engine::Postgres),
    ("psycopg2-binary", Engine::Postgres),
    ("psycopg-binary", Engine::Postgres),
    ("asyncpg", Engine::Postgres),
    ("pg8000", Engine::Postgres),
    ("mysqlclient", Engine::Mysql),
    ("pymysql", Engine::Mysql),
    ("mysql-connector-python", Engine::Mysql),
    ("aiomysql", Engine::Mysql),
    ("pymongo", Engine::Mongodb),
    ("motor", Engine::Mongodb),
    ("mongoengine", Engine::Mongodb),
    ("redis", Engine::Redis),
    ("django-redis", Engine::Redis),
    ("clickhouse-driver", Engine::Clickhouse),
    ("clickhouse-connect", Engine::Clickhouse),
    ("elasticsearch", Engine::Elasticsearch),
    ("pika", Engine::Rabbitmq),
    ("aio-pika", Engine::Rabbitmq),
];

const RUBY: &[(&str, Engine)] = &[
    ("pg", Engine::Postgres),
    ("mysql2", Engine::Mysql),
    ("trilogy", Engine::Mysql),
    ("sqlite3", Engine::Sqlite),
    ("redis", Engine::Redis),
    ("sidekiq", Engine::Redis),
    ("mongoid", Engine::Mongodb),
    ("dalli", Engine::Memcached),
    ("elasticsearch", Engine::Elasticsearch),
    ("bunny", Engine::Rabbitmq),
];

const PHP: &[(&str, Engine)] = &[
    ("ext-pdo_pgsql", Engine::Postgres),
    ("ext-pgsql", Engine::Postgres),
    ("ext-pdo_mysql", Engine::Mysql),
    ("ext-mysqli", Engine::Mysql),
    ("ext-pdo_sqlite", Engine::Sqlite),
    ("ext-redis", Engine::Redis),
    ("predis/predis", Engine::Redis),
    ("mongodb/mongodb", Engine::Mongodb),
    ("ext-mongodb", Engine::Mongodb),
];

const ELIXIR: &[(&str, Engine)] = &[
    ("postgrex", Engine::Postgres),
    ("myxql", Engine::Mysql),
    ("ecto_sqlite3", Engine::Sqlite),
    ("exqlite", Engine::Sqlite),
    ("redix", Engine::Redis),
    ("mongodb_driver", Engine::Mongodb),
];

const RUST: &[(&str, Engine)] = &[
    ("tokio-postgres", Engine::Postgres),
    ("postgres", Engine::Postgres),
    ("deadpool-postgres", Engine::Postgres),
    ("mysql", Engine::Mysql),
    ("mysql_async", Engine::Mysql),
    ("rusqlite", Engine::Sqlite),
    ("redis", Engine::Redis),
    ("mongodb", Engine::Mongodb),
    ("clickhouse", Engine::Clickhouse),
];

/// Go modules match by path prefix.
const GO: &[(&str, Engine)] = &[
    ("github.com/lib/pq", Engine::Postgres),
    ("github.com/jackc/pgx", Engine::Postgres),
    ("gorm.io/driver/postgres", Engine::Postgres),
    ("github.com/go-sql-driver/mysql", Engine::Mysql),
    ("gorm.io/driver/mysql", Engine::Mysql),
    ("github.com/redis/go-redis", Engine::Redis),
    ("github.com/go-redis/redis", Engine::Redis),
    ("github.com/gomodule/redigo", Engine::Redis),
    ("go.mongodb.org/mongo-driver", Engine::Mongodb),
    ("github.com/mattn/go-sqlite3", Engine::Sqlite),
    ("modernc.org/sqlite", Engine::Sqlite),
    ("github.com/ClickHouse/clickhouse-go", Engine::Clickhouse),
];

/// Build files (lowercased text) that name a driver.
const JVM: &[(&str, Engine)] = &[
    ("org.postgresql", Engine::Postgres),
    ("mysql-connector", Engine::Mysql),
    ("mariadb-java-client", Engine::Mariadb),
    ("spring-boot-starter-data-redis", Engine::Redis),
    ("jedis", Engine::Redis),
    ("lettuce", Engine::Redis),
    ("mongodb-driver", Engine::Mongodb),
    ("spring-boot-starter-data-mongodb", Engine::Mongodb),
];

/// The facts, and the `(project, engine, dependency)` triples they came from:
/// a dependency found twice in one project (two files naming it) is one fact.
#[derive(Default)]
struct Found {
    facts: Vec<DriverFact>,
    seen: HashSet<(String, Engine, String)>,
}

pub(super) fn detect(discovery: &Discovery) -> Vec<DriverFact> {
    let mut out = Found::default();
    for project in &discovery.projects {
        exact(project, &project.js_deps, JS, &mut out);
        exact(project, &project.py, PYTHON, &mut out);
        exact(project, &project.rb, RUBY, &mut out);
        exact(project, &project.php, PHP, &mut out);
        exact(project, &project.ex, ELIXIR, &mut out);
        exact(project, &project.rs, RUST, &mut out);
        rust_features(project, &mut out);
        for (prefix, engine) in GO {
            if let Some((dependency, evidence)) = project.go.with_prefix(prefix) {
                push(&mut out, project, *engine, dependency, evidence);
            }
        }
        for (needle, engine) in JVM {
            if let Some(file) = project.jvm.mentioning(needle).first() {
                push(&mut out, project, *engine, needle, file);
            }
        }
        if out.facts.len() >= MAX_DRIVERS {
            break;
        }
    }
    out.facts.truncate(MAX_DRIVERS);
    out.facts
}

fn exact(project: &Project, deps: &Deps, table: &[(&str, Engine)], out: &mut Found) {
    for (name, engine) in table {
        if let Some(evidence) = deps.evidence(name) {
            push(out, project, *engine, name, &evidence);
        }
    }
}

/// `sqlx`, `diesel` and `sea-orm` pick the engine through Cargo features.
fn rust_features(project: &Project, out: &mut Found) {
    for name in ["sqlx", "diesel", "sea-orm"] {
        let Some(evidence) = project.rs.evidence(name) else {
            continue;
        };
        for feature in project.rs.features(name) {
            let f = feature.to_ascii_lowercase();
            let engine = if f.contains("postgres") {
                Engine::Postgres
            } else if f.contains("mysql") {
                Engine::Mysql
            } else if f.contains("sqlite") {
                Engine::Sqlite
            } else {
                continue;
            };
            push(out, project, engine, name, &evidence);
        }
    }
}

fn push(out: &mut Found, project: &Project, engine: Engine, dependency: &str, evidence: &str) {
    if out
        .seen
        .insert((project.path.clone(), engine, dependency.to_string()))
    {
        out.facts.push(DriverFact {
            engine,
            evidence: vec![evidence.to_string()],
        });
    }
}

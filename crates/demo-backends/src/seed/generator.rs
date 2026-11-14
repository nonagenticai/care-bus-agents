//! ~500 plausible Dutch customers from a seeded ChaCha RNG. Same seed → byte-identical data.
use chrono::{Duration, NaiveDate, NaiveTime, TimeZone, Utc};
use rand::RngExt;
use rand::seq::IndexedRandom;
use rand_chacha::ChaCha8Rng;
use rand_chacha::rand_core::SeedableRng;
use sqlx::{PgConnection, Postgres, QueryBuilder};

use crate::Result;

pub const GENERATED_CUSTOMERS: usize = 500;

const FIRST: &[&str] = &[
    "Daan", "Sem", "Lucas", "Levi", "Finn", "Milan", "Noah", "Jesse", "Bram", "Thijs", "Emma",
    "Julia", "Mila", "Tess", "Sophie", "Zoë", "Sara", "Nora", "Fleur", "Lotte", "Anouk", "Iris",
    "Eva", "Lisa", "Ruben", "Tim", "Jasper", "Sanne", "Femke", "Maarten",
];
const LAST: &[&str] = &[
    "de Jong",
    "Jansen",
    "de Vries",
    "van den Berg",
    "van Dijk",
    "Bakker",
    "Janssen",
    "Visser",
    "Smit",
    "Meijer",
    "de Boer",
    "Mulder",
    "de Groot",
    "Bos",
    "Vos",
    "Peters",
    "Hendriks",
    "van Leeuwen",
    "Dekker",
    "Brouwer",
    "de Wit",
    "Dijkstra",
    "Smits",
    "de Graaf",
    "van der Meer",
];
const CITIES: &[&str] = &[
    "Amsterdam",
    "Rotterdam",
    "Den Haag",
    "Utrecht",
    "Eindhoven",
    "Groningen",
    "Tilburg",
    "Almere",
    "Breda",
    "Nijmegen",
    "Arnhem",
    "Haarlem",
    "Zwolle",
    "Leiden",
    "Maastricht",
];
// (sku, name, category, price in cents)
const PRODUCTS: &[(&str, &str, &str, i64)] = &[
    ("PH-HD9285", "Philips Airfryer XXL HD9285", "kitchen", 22900),
    ("SO-WH1000XM6", "Sony WH-1000XM6 headphones", "audio", 37900),
    (
        "BO-PSR18LI2",
        "Bosch PSR 18 LI-2 cordless drill",
        "tools",
        14900,
    ),
    (
        "DE-ECAM290",
        "De'Longhi Magnifica Evo coffee machine",
        "kitchen",
        44900,
    ),
    ("SA-QE55Q80", "Samsung 55\" QLED TV", "tv", 89900),
    ("AP-AIRPODS4", "Apple AirPods 4", "audio", 14900),
    ("DY-V15", "Dyson V15 Detect vacuum", "home", 69900),
    (
        "PH-SONIC9900",
        "Philips Sonicare 9900 toothbrush",
        "care",
        27900,
    ),
    ("ME-KG2000", "Makita DHP482 drill set", "tools", 19900),
    ("KI-K5PRO", "KitchenAid Artisan mixer", "kitchen", 54900),
    ("LO-MX3S", "Logitech MX Master 3S mouse", "computing", 10900),
    ("LE-LEGO10305", "LEGO Icons castle", "toys", 39999),
    ("GA-FR265", "Garmin Forerunner 265", "sports", 44999),
    ("JB-FLIP6", "JBL Flip 6 speaker", "audio", 12999),
];
const METHODS: &[&str] = &[
    "iDEAL",
    "iDEAL",
    "iDEAL",
    "Visa •• 1029",
    "Mastercard •• 7731",
    "PayPal",
    "Klarna",
];
const CARRIERS: &[&str] = &["PostNL", "PostNL", "DHL", "DPD"];

pub async fn insert(tx: &mut PgConnection, seed: u64, today: NaiveDate) -> Result<()> {
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    let (mut customers, mut orders, mut lines, mut ships, mut events, mut pays, mut comms) =
        (vec![], vec![], vec![], vec![], vec![], vec![], vec![]);
    let (mut ord_no, mut shp_no, mut pay_no) = (60001u32, 90001u32, 50001u32);
    for i in 0..GENERATED_CUSTOMERS {
        let cid = format!("C-{}", 20001 + i);
        let first = *FIRST.choose(&mut rng).unwrap();
        let last = *LAST.choose(&mut rng).unwrap();
        let email = format!(
            "{}.{}{}@example.nl",
            first.to_lowercase(),
            last.replace(' ', "").to_lowercase(),
            i
        );
        let tier = ["Standard", "Standard", "Standard", "Silver", "Gold"][rng.random_range(0..5)];
        let since = today - Duration::days(rng.random_range(30..3000));
        let city = *CITIES.choose(&mut rng).unwrap();
        let phone = format!(
            "+31 6 {:04} {:04}",
            rng.random_range(1000..9999),
            rng.random_range(1000..9999)
        );
        customers.push((
            cid.clone(),
            format!("{first} {last}"),
            email,
            phone,
            tier,
            city.to_string(),
            since,
        ));
        for _ in 0..rng.random_range(0..5) {
            let oid = format!("ORD-{ord_no}");
            ord_no += 1;
            let (sku, name, cat, price) = *PRODUCTS.choose(&mut rng).unwrap();
            let placed = today - Duration::days(rng.random_range(0..400));
            let age = (today - placed).num_days();
            let delivered_on = (age >= 4).then(|| placed + Duration::days(rng.random_range(1..4)));
            let status = if delivered_on.is_some() {
                "delivered"
            } else if age >= 1 {
                "in_transit"
            } else {
                "processing"
            };
            let sid = (status != "processing").then(|| {
                let s = format!("SHP-{shp_no}");
                shp_no += 1;
                s
            });
            orders.push((
                oid.clone(),
                cid.clone(),
                placed,
                name.to_string(),
                cat.to_string(),
                price,
                status,
                sid.clone(),
                delivered_on,
            ));
            lines.push((
                oid.clone(),
                1i32,
                sku.to_string(),
                name.to_string(),
                cat.to_string(),
                1i32,
                price,
            ));
            if let Some(sid) = &sid {
                let carrier = *CARRIERS.choose(&mut rng).unwrap();
                let at = |d: NaiveDate, h: u32, m: u32| {
                    Utc.from_utc_datetime(&d.and_time(NaiveTime::from_hms_opt(h, m, 0).unwrap()))
                };
                let shipped = placed + Duration::days(1);
                events.push((
                    sid.clone(),
                    1i32,
                    at(shipped.min(today), 18, rng.random_range(0..60)),
                    "Warehouse Tilburg".to_string(),
                    "Handed to carrier".to_string(),
                ));
                let delivered_at =
                    delivered_on.map(|d| at(d, rng.random_range(9..20), rng.random_range(0..60)));
                if let Some(da) = delivered_at {
                    events.push((
                        sid.clone(),
                        2i32,
                        da,
                        city.to_string(),
                        "Delivered".to_string(),
                    ));
                }
                let ship_status = if delivered_on.is_some() {
                    "delivered"
                } else {
                    "in_transit"
                };
                let eta = delivered_on
                    .is_none()
                    .then(|| format!("{}, 14:00–17:00", today + Duration::days(1)));
                ships.push((
                    sid.clone(),
                    oid.clone(),
                    carrier.to_string(),
                    ship_status,
                    eta,
                    delivered_at,
                    delivered_at.map(|_| format!("{}. {}", &first[..1], last)),
                ));
            }
            let method = *METHODS.choose(&mut rng).unwrap();
            let captured = Utc.from_utc_datetime(
                &placed.and_time(
                    NaiveTime::from_hms_opt(
                        rng.random_range(7..23),
                        rng.random_range(0..60),
                        rng.random_range(0..60),
                    )
                    .unwrap(),
                ),
            );
            pays.push((
                format!("PAY-{pay_no}"),
                oid.clone(),
                method.to_string(),
                price,
                captured,
            ));
            pay_no += 1;
            if rng.random_bool(0.02) {
                // A realistic duplicate capture (PAY-04 territory), seconds after the first.
                pays.push((
                    format!("PAY-{pay_no}"),
                    oid.clone(),
                    method.to_string(),
                    price,
                    captured + Duration::seconds(rng.random_range(10..120)),
                ));
                pay_no += 1;
            }
            comms.push((
                cid.clone(),
                placed,
                "email",
                format!("Order confirmation {oid}"),
            ));
        }
    }
    // Bulk insert in chunks (Postgres caps bind parameters at 65535).
    for chunk in customers.chunks(1000) {
        let mut q = QueryBuilder::<Postgres>::new(
            "INSERT INTO customers (customer_id,name,email,phone,tier,city,customer_since) ",
        );
        q.push_values(chunk, |mut b, c| {
            b.push_bind(&c.0)
                .push_bind(&c.1)
                .push_bind(&c.2)
                .push_bind(&c.3)
                .push_bind(c.4)
                .push_bind(&c.5)
                .push_bind(c.6);
        });
        q.build().execute(&mut *tx).await?;
    }
    for chunk in orders.chunks(1000) {
        let mut q = QueryBuilder::<Postgres>::new(
            "INSERT INTO orders (order_id,customer_id,placed,item,category,amount_cents,status,shipment_id,delivered_on) ",
        );
        q.push_values(chunk, |mut b, o| {
            b.push_bind(&o.0)
                .push_bind(&o.1)
                .push_bind(o.2)
                .push_bind(&o.3)
                .push_bind(&o.4)
                .push_bind(o.5)
                .push_bind(o.6)
                .push_bind(&o.7)
                .push_bind(o.8);
        });
        q.build().execute(&mut *tx).await?;
    }
    for chunk in lines.chunks(1000) {
        let mut q = QueryBuilder::<Postgres>::new(
            "INSERT INTO order_lines (order_id,line_no,sku,name,category,qty,unit_price_cents) ",
        );
        q.push_values(chunk, |mut b, l| {
            b.push_bind(&l.0)
                .push_bind(l.1)
                .push_bind(&l.2)
                .push_bind(&l.3)
                .push_bind(&l.4)
                .push_bind(l.5)
                .push_bind(l.6);
        });
        q.build().execute(&mut *tx).await?;
    }
    for chunk in ships.chunks(1000) {
        let mut q = QueryBuilder::<Postgres>::new(
            "INSERT INTO shipments (shipment_id,order_id,carrier,status,eta,delivered_at,signed_by) ",
        );
        q.push_values(chunk, |mut b, s| {
            b.push_bind(&s.0)
                .push_bind(&s.1)
                .push_bind(&s.2)
                .push_bind(s.3)
                .push_bind(&s.4)
                .push_bind(s.5)
                .push_bind(&s.6);
        });
        q.build().execute(&mut *tx).await?;
    }
    for chunk in events.chunks(1000) {
        let mut q = QueryBuilder::<Postgres>::new(
            "INSERT INTO shipment_events (shipment_id,seq,at,location,description) ",
        );
        q.push_values(chunk, |mut b, e| {
            b.push_bind(&e.0)
                .push_bind(e.1)
                .push_bind(e.2)
                .push_bind(&e.3)
                .push_bind(&e.4);
        });
        q.build().execute(&mut *tx).await?;
    }
    for chunk in pays.chunks(1000) {
        let mut q = QueryBuilder::<Postgres>::new(
            "INSERT INTO payments (payment_id,order_id,method,amount_cents,status,captured_at) ",
        );
        q.push_values(chunk, |mut b, p| {
            b.push_bind(&p.0)
                .push_bind(&p.1)
                .push_bind(&p.2)
                .push_bind(p.3)
                .push_bind("captured")
                .push_bind(p.4);
        });
        q.build().execute(&mut *tx).await?;
    }
    for chunk in comms.chunks(1000) {
        let mut q = QueryBuilder::<Postgres>::new(
            "INSERT INTO communications (customer_id,at,channel,subject) ",
        );
        q.push_values(chunk, |mut b, c| {
            b.push_bind(&c.0)
                .push_bind(c.1)
                .push_bind(c.2)
                .push_bind(&c.3);
        });
        q.build().execute(&mut *tx).await?;
    }
    Ok(())
}

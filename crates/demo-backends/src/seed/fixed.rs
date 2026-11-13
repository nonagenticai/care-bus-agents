//! The fixed demo customer every reset restores.
use sqlx::PgConnection;

use crate::Result;

pub const FIXED_CUSTOMER: &str = "C-10482";

pub async fn insert(tx: &mut PgConnection) -> Result<()> {
    let stmts = [
        "INSERT INTO customers VALUES ('C-10482','Anna de Vries','a.devries@example.nl','+31 6 1234 5678','Gold','en','Utrecht','2019-03-02')",
        "INSERT INTO orders VALUES
          ('ORD-58790','C-10482','2026-10-11','Philips Airfryer XXL HD9285','kitchen',22900,'in_transit','SHP-88123',NULL),
          ('ORD-58213','C-10482','2026-10-02','Sony WH-1000XM6 headphones','audio',37900,'delivered','SHP-87410','2026-10-05'),
          ('ORD-57102','C-10482','2026-06-14','Bosch PSR 18 LI-2 cordless drill','tools',14900,'delivered','SHP-80022','2026-06-18')",
        "INSERT INTO order_lines VALUES
          ('ORD-58790',1,'PH-HD9285','Philips Airfryer XXL HD9285','kitchen',1,22900),
          ('ORD-58213',1,'SO-WH1000XM6','Sony WH-1000XM6 headphones','audio',1,37900),
          ('ORD-57102',1,'BO-PSR18LI2','Bosch PSR 18 LI-2 cordless drill','tools',1,14900)",
        "INSERT INTO shipments VALUES
          ('SHP-88123','ORD-58790','PostNL','in_transit','2026-10-16, 14:00–17:00',NULL,NULL),
          ('SHP-87410','ORD-58213','PostNL','delivered',NULL,'2026-10-05 13:42:00+02','A. de Vries'),
          ('SHP-80022','ORD-57102','DHL','delivered',NULL,'2026-06-18 11:05:00+02','neighbour (nr. 14)')",
        "INSERT INTO shipment_events VALUES
          ('SHP-88123',1,'2026-10-12 18:40:00+02','Warehouse Tilburg','Handed to carrier'),
          ('SHP-88123',2,'2026-10-14 06:12:00+02','Sorting centre Nieuwegein','Sorted, waiting for route')",
        "INSERT INTO payments VALUES
          ('PAY-3398','ORD-58790','Visa •• 4417',22900,'captured','2026-10-11T19:02:11Z'),
          ('PAY-3399','ORD-58790','Visa •• 4417',22900,'captured','2026-10-11T19:02:49Z'),
          ('PAY-3120','ORD-58213','iDEAL',37900,'captured','2026-10-02T08:15:40Z'),
          ('PAY-2710','ORD-57102','iDEAL',14900,'captured','2026-06-14T20:41:03Z')",
        "INSERT INTO communications (customer_id, at, channel, subject) VALUES
          ('C-10482','2026-10-12','email','Your order ORD-58790 has shipped'),
          ('C-10482','2026-10-11','email','Order confirmation ORD-58790'),
          ('C-10482','2026-10-05','sms','ORD-58213 delivered')",
    ];
    for s in stmts {
        sqlx::query(s).execute(&mut *tx).await?;
    }
    Ok(())
}

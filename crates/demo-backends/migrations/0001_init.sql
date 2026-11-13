-- carebus_demo: fake backend systems. Money in integer cents.
CREATE TABLE customers (
  customer_id    text PRIMARY KEY,
  name           text NOT NULL,
  email          text NOT NULL,
  phone          text,
  tier           text NOT NULL CHECK (tier IN ('Standard','Silver','Gold')),
  language       text NOT NULL DEFAULT 'nl',
  city           text NOT NULL,
  customer_since date NOT NULL
);
CREATE TABLE orders (
  order_id     text PRIMARY KEY,
  customer_id  text NOT NULL REFERENCES customers ON DELETE CASCADE,
  placed       date NOT NULL,
  item         text NOT NULL,
  category     text NOT NULL,
  amount_cents bigint NOT NULL CHECK (amount_cents >= 0),
  status       text NOT NULL CHECK (status IN ('processing','in_transit','delivered','delayed','cancelled')),
  shipment_id  text,
  delivered_on date
);
CREATE INDEX orders_customer ON orders (customer_id, placed DESC);
CREATE TABLE order_lines (
  order_id         text NOT NULL REFERENCES orders ON DELETE CASCADE,
  line_no          int  NOT NULL,
  sku              text NOT NULL,
  name             text NOT NULL,
  category         text NOT NULL,
  qty              int  NOT NULL CHECK (qty > 0),
  unit_price_cents bigint NOT NULL,
  PRIMARY KEY (order_id, line_no)
);
CREATE TABLE shipments (
  shipment_id  text PRIMARY KEY,
  order_id     text NOT NULL REFERENCES orders ON DELETE CASCADE,
  carrier      text NOT NULL CHECK (carrier IN ('PostNL','DHL','DPD')),
  status       text NOT NULL CHECK (status IN ('label_created','in_transit','delayed','delivered')),
  eta          text,
  delivered_at timestamptz,
  signed_by    text
);
CREATE TABLE shipment_events (
  shipment_id text NOT NULL REFERENCES shipments ON DELETE CASCADE,
  seq         int  NOT NULL,
  at          timestamptz NOT NULL,
  location    text NOT NULL,
  description text NOT NULL,
  PRIMARY KEY (shipment_id, seq)
);
CREATE TABLE payments (
  payment_id   text PRIMARY KEY,
  order_id     text NOT NULL REFERENCES orders ON DELETE CASCADE,
  method       text NOT NULL,
  amount_cents bigint NOT NULL CHECK (amount_cents >= 0),
  status       text NOT NULL CHECK (status IN ('captured','declined','refunded')),
  captured_at  timestamptz NOT NULL
);
CREATE INDEX payments_order ON payments (order_id, captured_at);
CREATE SEQUENCE rma_seq START 41001;
CREATE SEQUENCE claim_seq START 7701;
CREATE SEQUENCE refund_seq START 90201;
CREATE TABLE refunds (
  refund_id       text PRIMARY KEY,
  payment_id      text NOT NULL REFERENCES payments ON DELETE CASCADE,
  amount_cents    bigint NOT NULL CHECK (amount_cents > 0),
  idempotency_key text NOT NULL UNIQUE,
  created_at      timestamptz NOT NULL DEFAULT now()
);
CREATE TABLE rmas (
  rma_id          text PRIMARY KEY,
  order_id        text NOT NULL REFERENCES orders ON DELETE CASCADE,
  reason          text NOT NULL,
  carrier         text NOT NULL DEFAULT 'PostNL',
  deadline        date NOT NULL,
  idempotency_key text NOT NULL UNIQUE,
  created_at      timestamptz NOT NULL DEFAULT now()
);
CREATE TABLE warranty_claims (
  claim_id        text PRIMARY KEY,
  order_id        text NOT NULL REFERENCES orders ON DELETE CASCADE,
  defect          text NOT NULL,
  next_step       text NOT NULL,
  idempotency_key text NOT NULL UNIQUE,
  created_at      timestamptz NOT NULL DEFAULT now()
);
CREATE TABLE communications (
  id          bigserial PRIMARY KEY,
  customer_id text NOT NULL REFERENCES customers ON DELETE CASCADE,
  at          date NOT NULL,
  channel     text NOT NULL CHECK (channel IN ('email','sms','chat')),
  subject     text NOT NULL
);

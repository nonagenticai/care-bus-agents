INSERT INTO context_spaces (key, area, label, request_type, required, entities, procedure) VALUES
 ('retail:order-support:returns','order-support','Returns','return','{order_id,reason}','{Customer,Order,Product,ReturnAuthorization}','return'),
 ('retail:order-support:warranty','order-support','Warranty claims','warranty_claim','{order_id,defect}','{Customer,Order,Product,WarrantyClaim}','warranty_claim'),
 ('retail:logistics:delivery-status','logistics','Delivery status','delivery_status','{order_id}','{Order,Shipment,Carrier}','delivery_status'),
 ('retail:payments:payment-errors','payments','Payment errors','payment_error','{order_id,error_type}','{Customer,Order,Payment,Refund}','duplicate_charge_refund'),
 ('retail:faq:general','faq','General questions','faq','{question}','{Policy}','faq_answer');

INSERT INTO ontology_entities (name, scope, industry, department, label, synonyms) VALUES
 ('Customer','generic',NULL,NULL,'Customer','{client,buyer,cust_no}'),
 ('Communication','generic',NULL,NULL,'Communication','{message,email,sms}'),
 ('Policy','generic',NULL,NULL,'Policy','{terms,rule}'),
 ('Order','industry','retail',NULL,'Order','{purchase,ord_no,bestelling}'),
 ('Product','industry','retail',NULL,'Product','{item,article,sku}'),
 ('Payment','industry','retail',NULL,'Payment','{charge,transaction,psp_reference}'),
 ('Shipment','industry','retail',NULL,'Shipment','{parcel,package,zending}'),
 ('ReturnAuthorization','industry_dept','retail','order-support','Return (RMA)','{rma,"return label",retour}'),
 ('WarrantyClaim','industry_dept','retail','order-support','Warranty claim','{"guarantee claim",repair}'),
 ('Carrier','industry_dept','retail','logistics','Carrier','{PostNL,DHL,courier}'),
 ('Refund','industry_dept','retail','payments','Refund','{"money back",chargeback,terugbetaling}');

INSERT INTO ontology_attributes (entity, name, data_type, pii) VALUES
 ('Customer','customer_id','string',false),('Customer','name','string',true),('Customer','email','string',true),('Customer','tier','enum',false),
 ('Communication','channel','enum',false),('Communication','subject','string',false),('Communication','at','date',false),
 ('Policy','code','string',false),('Policy','text','text',false),
 ('Order','order_id','string',false),('Order','placed','date',false),('Order','amount','money',false),('Order','status','enum',false),('Order','shipment_id','string',false),
 ('Product','sku','string',false),('Product','name','string',false),('Product','category','enum',false),
 ('Payment','payment_id','string',false),('Payment','method','string',true),('Payment','amount','money',false),('Payment','status','enum',false),('Payment','at','datetime',false),
 ('Shipment','shipment_id','string',false),('Shipment','carrier','enum',false),('Shipment','status','enum',false),('Shipment','eta','string',false),
 ('ReturnAuthorization','rma_id','string',false),('ReturnAuthorization','reason','text',false),('ReturnAuthorization','deadline','date',false),
 ('WarrantyClaim','claim_id','string',false),('WarrantyClaim','defect','text',false),('WarrantyClaim','warranty_until','date',false),
 ('Carrier','code','string',false),('Carrier','name','string',false),
 ('Refund','refund_id','string',false),('Refund','amount','money',false),('Refund','eta','string',false);

INSERT INTO ontology_relations (subject, predicate, object, cardinality) VALUES
 ('Customer','places','Order','1:n'),('Order','contains','Product','n:m'),('Order','paidBy','Payment','1:n'),
 ('Order','shippedVia','Shipment','1:n'),('Shipment','carriedBy','Carrier','n:1'),('ReturnAuthorization','for','Order','n:1'),
 ('WarrantyClaim','covers','Product','n:1'),('Refund','reverses','Payment','n:1'),('Customer','receives','Communication','1:n'),
 ('Policy','governs','ReturnAuthorization','1:n');

INSERT INTO model_routes (use_name, provider, model, base_url, secret_ref, timeout_ms, options) VALUES
 ('generation','vllm','local-llm','https://api.nonagentic.ai/vllm/v1','carebus-llm/vllm-license-token',60000,'{}'),
 ('routing','laya','jev-latest','https://api.nonagentic.ai/laya','carebus-llm/laya-license-token',800,'{}'),
 ('embeddings','strata_rag','BAAI/bge-m3',NULL,NULL,5000,'{"dimension":1024}'),
 ('rerank','strata_rag','BAAI/bge-reranker-base',NULL,NULL,5000,'{"candidates":8,"max_tokens":256}'),
 ('vision','vlm_backend','mlx-community/Qwen2-VL-2B-Instruct-4bit','http://vlm.example.com:9203/api/vlm',NULL,30000,'{"dormant_since":"2026-05-27","max_image_px":2073600,"strip_exif":true}');

-- Procedures: version + PDDL step model (procedure_management vocabulary). Plans equal carebus::domain plans.
WITH p AS (
  INSERT INTO procedures (name, version, version_status, context_space, reviewers, tool_requirements, success_criteria, plan) VALUES
   ('return','1.3.0','PUBLISHED','retail:order-support:returns','{a.bakker,r.visser}',
    '{crm_verify_customer,order_get_order,rules_check_return_window,procedures_get_procedure,order_create_rma}','{"(rma-created ?o)"}',
    '{"phases":[{"steps":[{"worker":"crm","action":"verify_customer","as":"customer","args":{"customer_id":"$request.customer_id"}},{"worker":"order","action":"get_order","as":"order","args":{"order_id":"$request.order_id"}}],"gate":null},{"steps":[{"worker":"business-rules","action":"check_return_window","as":"rules","args":{"order_id":"$order.order_id","category":"$order.category","delivered_on":"$order.delivered_on"}},{"worker":"procedures","action":"get_procedure","as":"procedure","args":{"name":"return"}}],"gate":"rules"},{"steps":[{"worker":"order","action":"create_rma","as":"rma","args":{"order_id":"$request.order_id","reason":"$request.reason"}}],"gate":null}]}'),
   ('return','1.4.0-beta','CANARY','retail:order-support:returns','{a.bakker}',
    '{crm_verify_customer,order_get_order,rules_check_return_window,procedures_get_procedure,order_create_rma}','{"(rma-created ?o)"}','{"phases":[{"steps":[{"worker":"crm","action":"verify_customer","as":"customer","args":{"customer_id":"$request.customer_id"}},{"worker":"order","action":"get_order","as":"order","args":{"order_id":"$request.order_id"}}],"gate":null},{"steps":[{"worker":"business-rules","action":"check_return_window","as":"rules","args":{"order_id":"$order.order_id","category":"$order.category","delivered_on":"$order.delivered_on"}},{"worker":"procedures","action":"get_procedure","as":"procedure","args":{"name":"return"}}],"gate":"rules"},{"steps":[{"worker":"order","action":"create_rma","as":"rma","args":{"order_id":"$request.order_id","reason":"$request.reason"}}],"gate":null}]}'),
   ('warranty_claim','1.1.0','PUBLISHED','retail:order-support:warranty','{a.bakker,r.visser}',
    '{crm_verify_customer,order_get_order,rules_check_warranty,procedures_get_procedure,order_create_warranty_claim}','{"(claim-registered ?o)"}',
    '{"phases":[{"steps":[{"worker":"crm","action":"verify_customer","as":"customer","args":{"customer_id":"$request.customer_id"}},{"worker":"order","action":"get_order","as":"order","args":{"order_id":"$request.order_id"}}],"gate":null},{"steps":[{"worker":"business-rules","action":"check_warranty","as":"rules","args":{"order_id":"$order.order_id","category":"$order.category","delivered_on":"$order.delivered_on"}},{"worker":"procedures","action":"get_procedure","as":"procedure","args":{"name":"warranty_claim"}}],"gate":"rules"},{"steps":[{"worker":"order","action":"create_warranty_claim","as":"claim","args":{"order_id":"$request.order_id","defect":"$request.defect"}}],"gate":null}]}'),
   ('delivery_status','2.0.0','PUBLISHED','retail:logistics:delivery-status','{r.visser,a.bakker}',
    '{order_get_order,delivery_track_shipment}','{"(shipment-status-known ?o)"}',
    '{"phases":[{"steps":[{"worker":"order","action":"get_order","as":"order","args":{"order_id":"$request.order_id"}}],"gate":null},{"steps":[{"worker":"delivery","action":"track_shipment","as":"shipment","args":{"shipment_id":"$order.shipment_id"}}],"gate":null}]}'),
   ('duplicate_charge_refund','1.0.2','PUBLISHED','retail:payments:payment-errors','{a.bakker,k.dekker}',
    '{crm_verify_customer,order_get_payments,rules_check_payment_issue,procedures_get_procedure,order_issue_refund}','{"(refunded ?p)"}',
    '{"phases":[{"steps":[{"worker":"crm","action":"verify_customer","as":"customer","args":{"customer_id":"$request.customer_id"}},{"worker":"order","action":"get_payments","as":"payments","args":{"order_id":"$request.order_id"}}],"gate":null},{"steps":[{"worker":"business-rules","action":"check_payment_issue","as":"rules","args":{"order_id":"$request.order_id","error_type":"$request.error_type"}},{"worker":"procedures","action":"get_procedure","as":"procedure","args":{"name":"duplicate_charge_refund"}}],"gate":"rules"},{"steps":[{"worker":"order","action":"issue_refund","as":"refund","args":{"payment_id":"$rules.refund_payment_id","amount":"$rules.refund_amount"}}],"gate":null}]}'),
   ('faq_answer','2.0.0','PUBLISHED','retail:faq:general','{m.jansen,a.bakker}','{faq_ask}','{"(answered ?q)"}',
    '{"phases":[{"steps":[{"worker":"faq","action":"ask","as":"faq","args":{"question":"$request.question"}}],"gate":null}]}')
  RETURNING id, name, version)
INSERT INTO procedure_steps (procedure_id, step_number, phase, name, action_type, worker, mcp_tool, performed_by, preconditions, postconditions, parameters, gate)
SELECT p.id, s.n, s.ph, s.nm, s.act, s.w, s.tool, s.role, s.pre::text[], s.post::text[], s.params::jsonb, s.gate
FROM p JOIN (VALUES
 ('return','1.3.0',1,0,'Verify customer','verify-customer','crm','crm_verify_customer','crm worker','{"(customer ?c)"}','{"(verified ?c)"}','{"?customer_id":"customer_id"}',false),
 ('return','1.3.0',2,0,'Get order','get-order','order','order_get_order','order worker','{"(order ?o)"}','{"(order-known ?o)"}','{"?order_id":"order_id"}',false),
 ('return','1.3.0',3,1,'Check return window','check-return-window','business-rules','rules_check_return_window','business-rules worker','{"(order-known ?o)","(delivered ?o)"}','{"(within-return-window ?o)"}','{"?order_id":"order_id","?category":"category","?delivered_on":"delivered_on"}',true),
 ('return','1.3.0',4,1,'Load procedure','get-procedure','procedures','procedures_get_procedure','procedures worker','{}','{"(procedure-loaded ?n)"}','{"?name":"name"}',false),
 ('return','1.3.0',5,2,'Create RMA','create-rma','order','order_create_rma','order worker','{"(verified ?c)","(within-return-window ?o)"}','{"(rma-created ?o)"}','{"?order_id":"order_id","?reason":"reason"}',false),
 ('warranty_claim','1.1.0',1,0,'Verify customer','verify-customer','crm','crm_verify_customer','crm worker','{"(customer ?c)"}','{"(verified ?c)"}','{"?customer_id":"customer_id"}',false),
 ('warranty_claim','1.1.0',2,0,'Get order','get-order','order','order_get_order','order worker','{"(order ?o)"}','{"(order-known ?o)"}','{"?order_id":"order_id"}',false),
 ('warranty_claim','1.1.0',3,1,'Check warranty','check-warranty','business-rules','rules_check_warranty','business-rules worker','{"(order-known ?o)","(delivered ?o)"}','{"(under-warranty ?o)"}','{"?order_id":"order_id","?category":"category","?delivered_on":"delivered_on"}',true),
 ('warranty_claim','1.1.0',4,1,'Load procedure','get-procedure','procedures','procedures_get_procedure','procedures worker','{}','{"(procedure-loaded ?n)"}','{"?name":"name"}',false),
 ('warranty_claim','1.1.0',5,2,'Register claim','create-warranty-claim','order','order_create_warranty_claim','order worker','{"(verified ?c)","(under-warranty ?o)"}','{"(claim-registered ?o)"}','{"?order_id":"order_id","?defect":"defect"}',false),
 ('delivery_status','2.0.0',1,0,'Get order','get-order','order','order_get_order','order worker','{"(order ?o)"}','{"(order-known ?o)"}','{"?order_id":"order_id"}',false),
 ('delivery_status','2.0.0',2,1,'Track shipment','track-shipment','delivery','delivery_track_shipment','delivery worker','{"(order-known ?o)"}','{"(shipment-status-known ?o)"}','{"?shipment_id":"shipment_id"}',false),
 ('duplicate_charge_refund','1.0.2',1,0,'Verify customer','verify-customer','crm','crm_verify_customer','crm worker','{"(customer ?c)"}','{"(verified ?c)"}','{"?customer_id":"customer_id"}',false),
 ('duplicate_charge_refund','1.0.2',2,0,'Get payments','get-payments','order','order_get_payments','order worker','{"(order-known ?o)"}','{"(payments-known ?o)"}','{"?order_id":"order_id"}',false),
 ('duplicate_charge_refund','1.0.2',3,1,'Check payment issue','check-payment-issue','business-rules','rules_check_payment_issue','business-rules worker','{"(payments-known ?o)"}','{"(duplicate-capture ?p)"}','{"?order_id":"order_id","?error_type":"error_type"}',true),
 ('duplicate_charge_refund','1.0.2',4,1,'Load procedure','get-procedure','procedures','procedures_get_procedure','procedures worker','{}','{"(procedure-loaded ?n)"}','{"?name":"name"}',false),
 ('duplicate_charge_refund','1.0.2',5,2,'Issue refund','issue-refund','order','order_issue_refund','order worker','{"(verified ?c)","(duplicate-capture ?p)"}','{"(refunded ?p)"}','{"?payment_id":"payment_id","?amount":"amount"}',false),
 ('faq_answer','2.0.0',1,0,'Answer from knowledge stores','ask','faq','faq_ask','faq worker','{}','{"(answered ?q)"}','{"?question":"question"}',false)
) AS s(name, version, n, ph, nm, act, w, tool, role, pre, post, params, gate) ON s.name = p.name AND s.version = p.version;

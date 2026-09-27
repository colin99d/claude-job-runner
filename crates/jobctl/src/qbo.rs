//! QuickBooks Online tools, the Rust counterpart of the chat application's
//! QuickBooks features (`quickbooks.server.ts`, `qboClassSales.ts`,
//! `qboCustomerSales.ts`, `qboCustomerInvoices.server.ts`).
//!
//! The runner hands each job one access token for the asking user's
//! company (see the daemon's `qbo` module), through these variables:
//!
//! * `QBO_ACCESS_TOKEN`, `QBO_REALM_ID`, `QBO_API_BASE`: the account;
//! * `QBO_COMPANY_ID`, `QBO_USER_ID`, `QBO_REQUESTER_IS_ADMIN`: who asks.
//!
//! The token only opens that company's QuickBooks account, so tenant
//! isolation does not depend on the model. On top of it the tools apply the
//! app's own rules: every employee may look up customers, a CRM customer's
//! invoices, invoices of customers linked to the CRM, and their own sales;
//! company-wide figures and free-form queries are for admins.
//!
//! Everything here is read-only: nothing is written to QuickBooks or to the
//! database (the app remembers a matched `customers.qbo_id`; this does not).

use std::collections::{BTreeMap, HashSet};
use std::fmt::Write as _;
use std::time::Duration;

use serde_json::{Map, Value, json};
use sqlx::AssertSqlSafe;
use sqlx::mysql::MySqlConnection;

use crate::sql::{MAX_OUTPUT_CHARS, truncate};
use crate::{Error, Result};

/// QuickBooks API minor version, the same as the app's.
const MINOR_VERSION: &str = "75";

/// Page size of QuickBooks queries (its maximum).
const PAGE_SIZE: usize = 1000;

/// Upper bound on pages fetched for one date range.
const MAX_PAGES: usize = 10;

const HTTP_TIMEOUT: Duration = Duration::from_secs(60);

/// Who is asking, from the runner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Scope {
    /// `company.id` whose QuickBooks account the token opens.
    pub company_id: i64,
    /// `users.id` of the person asking.
    pub user_id: i64,
    /// Whether they may see company-wide figures.
    pub is_admin: bool,
}

/// One job's QuickBooks connection.
#[derive(Debug, Clone)]
pub struct Session {
    http: reqwest::Client,
    company_url: String,
    token: String,
    scope: Scope,
}

impl Session {
    /// The session the runner configured, or `None` when this job has no
    /// QuickBooks access.
    pub fn from_env() -> Result<Option<Self>> {
        let var = |name: &str| std::env::var(name).ok().filter(|v| !v.trim().is_empty());
        let Some(token) = var("QBO_ACCESS_TOKEN") else {
            return Ok(None);
        };
        let missing = |name: &str| Error::Config(format!("{name} is not set"));
        let realm_id = var("QBO_REALM_ID").ok_or_else(|| missing("QBO_REALM_ID"))?;
        let api_base = var("QBO_API_BASE").ok_or_else(|| missing("QBO_API_BASE"))?;
        let id = |name: &str| -> Result<i64> {
            var(name)
                .ok_or_else(|| missing(name))?
                .trim()
                .parse()
                .map_err(|_| Error::Config(format!("{name} is not a number")))
        };
        let scope = Scope {
            company_id: id("QBO_COMPANY_ID")?,
            user_id: id("QBO_USER_ID")?,
            is_admin: var("QBO_REQUESTER_IS_ADMIN").as_deref() == Some("1"),
        };
        Ok(Some(Self::new(&api_base, &realm_id, &token, scope)?))
    }

    /// A session for `realm_id` at `api_base` (`https://...intuit.com`).
    pub fn new(api_base: &str, realm_id: &str, token: &str, scope: Scope) -> Result<Self> {
        if !is_entity_id(realm_id) {
            return Err(Error::Config(format!(
                "invalid QuickBooks realm id {realm_id:?}"
            )));
        }
        install_crypto_provider();
        let http = reqwest::Client::builder()
            .timeout(HTTP_TIMEOUT)
            .build()
            .map_err(|err| Error::Http(format!("cannot build HTTP client: {err}")))?;
        Ok(Self {
            http,
            company_url: format!("{}/v3/company/{realm_id}", api_base.trim_end_matches('/')),
            token: token.to_owned(),
            scope,
        })
    }

    /// Who this session acts for.
    #[must_use]
    pub const fn scope(&self) -> Scope {
        self.scope
    }

    /// Whether this requester may run tool `name` (without the `qbo_`
    /// prefix); `None` if there is no such tool.
    #[must_use]
    pub fn authorize(&self, name: &str) -> Option<Result<()>> {
        let tool = TOOLS.iter().find(|tool| tool.name == name)?;
        Some(if tool.admin_only && !self.scope.is_admin {
            Err(Error::Denied(
                "only admins can use this QuickBooks tool".to_owned(),
            ))
        } else {
            Ok(())
        })
    }

    /// Runs tool `name` (without the `qbo_` prefix); `None` if there is no
    /// such tool. The text is JSON, cut to [`MAX_OUTPUT_CHARS`].
    pub async fn call(
        &self,
        conn: &mut MySqlConnection,
        name: &str,
        args: &Value,
    ) -> Option<Result<String>> {
        if let Err(err) = self.authorize(name)? {
            return Some(Err(err));
        }
        let outcome = match name {
            "search_customers" => self.search_customers(conn, args).await,
            "customer_invoices" => self.customer_invoices(conn, args).await,
            "invoice" => self.invoice(conn, args).await,
            "sales_rep" => self.sales_rep(conn, args).await,
            "invoices_in_range" => self.invoices_in_range(args).await,
            "class_sales" => self.class_sales(args).await,
            "query" => self.query(args).await,
            "report" => self.report(args).await,
            _ => return None,
        };
        Some(outcome.map(|value| truncate(value.to_string(), MAX_OUTPUT_CHARS)))
    }

    // --- tools ---------------------------------------------------------------

    async fn search_customers(&self, conn: &mut MySqlConnection, args: &Value) -> Result<Value> {
        let name = required_str(args, "name")?;
        let customers = self.customers_by_name(&name).await?;
        let ids: Vec<&str> = customers.iter().map(|c| c.id.as_str()).collect();
        let linked = linked_crm_customers(conn, self.scope.company_id, &ids).await?;
        Ok(Value::Array(
            customers
                .iter()
                .map(|c| {
                    json!({
                        "qboCustomerId": c.id,
                        "displayName": c.display_name,
                        "email": c.email,
                        "crmCustomers": linked.get(&c.id).cloned().unwrap_or_default(),
                    })
                })
                .collect(),
        ))
    }

    async fn customer_invoices(&self, conn: &mut MySqlConnection, args: &Value) -> Result<Value> {
        let customer_id = optional_positive(args, "customer_id")?;
        let qbo_customer_id = optional_entity_id(args, "qbo_customer_id")?;
        match (customer_id, qbo_customer_id) {
            (Some(customer_id), None) => self.crm_customer_invoices(conn, customer_id).await,
            (None, Some(qbo_id)) => {
                let linked = linked_crm_customers(conn, self.scope.company_id, &[&qbo_id]).await?;
                if !self.scope.is_admin && !linked.contains_key(&qbo_id) {
                    return Err(Error::Denied(
                        "this QuickBooks customer is not linked to a CRM customer; only admins \
                         can see it"
                            .to_owned(),
                    ));
                }
                Ok(json!({
                    "qboCustomerId": qbo_id,
                    "crmCustomers": linked.get(&qbo_id).cloned().unwrap_or_default(),
                    "invoices": self.invoices_for_customer(&qbo_id).await?,
                }))
            }
            _ => Err(Error::InvalidArgument(
                "pass exactly one of customer_id (CRM) or qbo_customer_id".to_owned(),
            )),
        }
    }

    async fn crm_customer_invoices(
        &self,
        conn: &mut MySqlConnection,
        customer_id: i64,
    ) -> Result<Value> {
        // id, name, qbo_id, email
        type CrmCustomer = (i64, Option<String>, Option<i64>, Option<String>);
        let row: Option<CrmCustomer> = sqlx::query_as(
            "SELECT CAST(c.id AS SIGNED), CAST(c.name AS CHAR), CAST(c.qbo_id AS SIGNED), \
                    CAST(ce.email AS CHAR) \
               FROM customers c \
               LEFT JOIN customers_emails ce ON ce.id = c.email_id AND ce.customer_id = c.id \
              WHERE c.id = ? AND c.company_id = ? AND c.deleted_at IS NULL \
              LIMIT 1",
        )
        .bind(customer_id)
        .bind(self.scope.company_id)
        .fetch_optional(&mut *conn)
        .await?;
        let Some((id, name, qbo_id, email)) = row else {
            return Err(Error::NotFound(format!("customer {customer_id} not found")));
        };
        let name = name.unwrap_or_default();
        let email = email.unwrap_or_default();

        let (qbo_id, matched_by) = match qbo_id.filter(|id| *id > 0) {
            Some(qbo_id) => (qbo_id.to_string(), "customers.qbo_id"),
            None => self
                .resolve_customer(name.trim(), email.trim())
                .await?
                .ok_or_else(|| {
                    Error::NotFound(format!(
                        "customer {id} is not linked to QuickBooks and no QuickBooks customer \
                     matches their email or name"
                    ))
                })?,
        };
        Ok(json!({
            "customerId": id,
            "customerName": name,
            "qboCustomerId": qbo_id,
            "matchedBy": matched_by,
            "invoices": self.invoices_for_customer(&qbo_id).await?,
        }))
    }

    async fn invoice(&self, conn: &mut MySqlConnection, args: &Value) -> Result<Value> {
        let invoice_id = optional_entity_id(args, "invoice_id")?
            .ok_or_else(|| Error::InvalidArgument("invoice_id is required".to_owned()))?;
        let payload = self.get(&format!("invoice/{invoice_id}"), &[]).await?;
        let invoice = payload.get("Invoice").cloned().unwrap_or(payload);
        let customer = invoice["CustomerRef"]["value"].as_str().map(str::to_owned);
        let linked = match &customer {
            Some(id) => linked_crm_customers(conn, self.scope.company_id, &[id]).await?,
            None => BTreeMap::new(),
        };
        let crm = customer.as_ref().and_then(|id| linked.get(id)).cloned();
        if !self.scope.is_admin && crm.is_none() {
            return Err(Error::Denied(
                "this invoice's customer is not linked to a CRM customer; only admins can see it"
                    .to_owned(),
            ));
        }
        Ok(json!({ "crmCustomers": crm.unwrap_or_default(), "invoice": invoice }))
    }

    async fn sales_rep(&self, conn: &mut MySqlConnection, args: &Value) -> Result<Value> {
        let start = required_date(args, "start_date")?;
        let end = required_date(args, "end_date")?;
        let own: Option<(Option<String>,)> = sqlx::query_as(
            "SELECT CAST(name AS CHAR) FROM users WHERE id = ? AND company_id = ? LIMIT 1",
        )
        .bind(self.scope.user_id)
        .bind(self.scope.company_id)
        .fetch_optional(&mut *conn)
        .await?;
        let own = own.and_then(|(name,)| name).unwrap_or_default();
        let rep = optional_str(args, "rep_name").unwrap_or_else(|| own.trim().to_owned());
        if !self.scope.is_admin && !rep.eq_ignore_ascii_case(own.trim()) {
            return Err(Error::Denied(
                "only admins can see another sales rep's sales".to_owned(),
            ));
        }
        if rep.is_empty() {
            return Err(Error::InvalidArgument(
                "the sales rep has no name to match QuickBooks classes by".to_owned(),
            ));
        }

        let class_sales = self.class_sales_report(&start, &end).await?;
        let amount = sum_for_rep(&class_sales.rows, &rep);
        let classes = self.classes().await?;
        let class_ids = class_ids_for_rep(&classes, &rep);
        let (customers, total) = if class_ids.is_empty() {
            (Vec::new(), 0.0)
        } else {
            let report = parse_report(
                &self
                    .get(
                        "reports/CustomerSales",
                        &[
                            ("accounting_method", "Accrual".to_owned()),
                            ("summarize_column_by", "Total".to_owned()),
                            ("start_date", start.clone()),
                            ("end_date", end.clone()),
                            ("class", class_ids.join(",")),
                        ],
                    )
                    .await?,
            );
            let customers = attach_invoices(
                customer_sales_rows(&report),
                &self.invoices_between(&start, &end).await?,
            );
            let total = report
                .total
                .unwrap_or_else(|| total(customers.iter().map(|c| c.amount)));
            (customers, total)
        };
        Ok(json!({
            "repName": rep,
            "startDate": class_sales.start_period.unwrap_or(start),
            "endDate": class_sales.end_period.unwrap_or(end),
            "currency": class_sales.currency.unwrap_or_else(|| "USD".to_owned()),
            "classSalesAmount": amount,
            "matchedClasses": classes
                .iter()
                .filter(|c| class_ids.contains(&c.id))
                .map(|c| c.fully_qualified_name.clone())
                .collect::<Vec<_>>(),
            "customerSalesTotal": total,
            "customers": customers.iter().map(CustomerSales::to_json).collect::<Vec<_>>(),
        }))
    }

    async fn invoices_in_range(&self, args: &Value) -> Result<Value> {
        let start = required_date(args, "start_date")?;
        let end = required_date(args, "end_date")?;
        let invoices = self.invoices_between(&start, &end).await?;
        Ok(json!({
            "startDate": start,
            "endDate": end,
            "count": invoices.len(),
            "totalAmt": total(invoices.iter().filter_map(|i| number(&i["TotalAmt"]))),
            "openBalance": total(invoices.iter().filter_map(|i| number(&i["Balance"]))),
            "invoices": invoices.iter().map(invoice_summary).collect::<Vec<_>>(),
        }))
    }

    async fn class_sales(&self, args: &Value) -> Result<Value> {
        let start = required_date(args, "start_date")?;
        let end = required_date(args, "end_date")?;
        let report = self.class_sales_report(&start, &end).await?;
        Ok(json!({
            "startDate": report.start_period,
            "endDate": report.end_period,
            "currency": report.currency,
            "total": report.total,
            "rows": omit_zero_rows(&report.rows).iter().map(Row::to_json).collect::<Vec<_>>(),
        }))
    }

    async fn query(&self, args: &Value) -> Result<Value> {
        let query = required_str(args, "query")?;
        if !query
            .get(..7)
            .is_some_and(|head| head.eq_ignore_ascii_case("select "))
        {
            return Err(Error::InvalidArgument(
                "only SELECT queries are allowed".to_owned(),
            ));
        }
        let payload = self.get("query", &[("query", query)]).await?;
        Ok(payload.get("QueryResponse").cloned().unwrap_or(payload))
    }

    async fn report(&self, args: &Value) -> Result<Value> {
        let name = required_str(args, "report")?;
        if !(1..=64).contains(&name.len()) || !name.chars().all(|c| c.is_ascii_alphabetic()) {
            return Err(Error::InvalidArgument(
                "report must be a report name such as ProfitAndLoss".to_owned(),
            ));
        }
        let mut params = Vec::new();
        if let Some(map) = args.get("params").and_then(Value::as_object) {
            for (key, value) in map {
                let value = match value {
                    Value::String(text) => text.clone(),
                    Value::Number(number) => number.to_string(),
                    Value::Bool(flag) => flag.to_string(),
                    _ => {
                        return Err(Error::InvalidArgument(format!(
                            "report parameter {key} must be a string or number"
                        )));
                    }
                };
                params.push((key.as_str(), value));
            }
        }
        self.get(&format!("reports/{name}"), &params).await
    }

    // --- QuickBooks API --------------------------------------------------------

    /// `GET {company}/{path}?{params}&minorversion=75`, failing on HTTP errors
    /// and on QuickBooks `Fault` bodies with QuickBooks' own message.
    async fn get(&self, path: &str, params: &[(&str, String)]) -> Result<Value> {
        let mut url = format!("{}/{path}?", self.company_url);
        for (key, value) in params {
            let _ = write!(url, "{}={}&", percent_encode(key), percent_encode(value));
        }
        url.push_str("minorversion=");
        url.push_str(MINOR_VERSION);

        let response = self
            .http
            .get(&url)
            .bearer_auth(&self.token)
            .header("Accept", "application/json")
            .send()
            .await
            .map_err(|err| Error::Http(format!("QuickBooks {path}: {err}")))?;
        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|err| Error::Http(format!("QuickBooks {path}: {err}")))?;
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(Error::Http(
                "QuickBooks rejected the access token; ask the user to try again, and an admin \
                 to reconnect QuickBooks if it keeps happening"
                    .to_owned(),
            ));
        }
        let payload: Value = serde_json::from_str(&body).map_err(|_| {
            Error::Http(format!(
                "QuickBooks {path}: HTTP {status}: {}",
                truncate(body.clone(), 500)
            ))
        })?;
        if let Some(fault) = payload.get("Fault") {
            return Err(Error::Http(format!("QuickBooks {path}: {fault}")));
        }
        if !status.is_success() {
            return Err(Error::Http(format!(
                "QuickBooks {path}: HTTP {status}: {payload}"
            )));
        }
        Ok(payload)
    }

    async fn run_query(&self, query: String) -> Result<Value> {
        self.get("query", &[("query", query)]).await
    }

    async fn customers_by_name(&self, name: &str) -> Result<Vec<QboCustomer>> {
        let payload = self
            .run_query(format!(
                "select Id, DisplayName, PrimaryEmailAddr from Customer \
                 where DisplayName like '%{}%' maxresults 25",
                escape(name)
            ))
            .await?;
        Ok(entities(&payload, "Customer")
            .iter()
            .filter_map(QboCustomer::parse)
            .collect())
    }

    /// The app's `resolveQboCustomerId`: a unique email match, else the best
    /// name match.
    async fn resolve_customer(
        &self,
        name: &str,
        email: &str,
    ) -> Result<Option<(String, &'static str)>> {
        if !email.is_empty() {
            let payload = self
                .run_query(format!(
                    "select * from Customer WHERE PrimaryEmailAddr = '{}'",
                    escape(email)
                ))
                .await?;
            let matches = entities(&payload, "Customer");
            if let [only] = matches.as_slice()
                && let Some(customer) = QboCustomer::parse(only)
            {
                return Ok(Some((customer.id, "email")));
            }
        }
        if name.is_empty() {
            return Ok(None);
        }
        let matches = self.customers_by_name(name).await?;
        Ok(pick_by_name(&matches, name, email).map(|id| (id, "name")))
    }

    async fn invoices_for_customer(&self, qbo_customer_id: &str) -> Result<Vec<Value>> {
        let payload = self
            .run_query(format!(
                "select Id, DocNumber, TxnDate, DueDate, TotalAmt, Balance, CurrencyRef, ShipAddr, \
                 CustomerRef from Invoice where CustomerRef = '{}' MAXRESULTS 1000",
                escape(qbo_customer_id)
            ))
            .await?;
        let mut invoices: Vec<Value> = entities(&payload, "Invoice")
            .iter()
            .map(invoice_summary)
            .collect();
        invoices.sort_by(|a, b| {
            b["txnDate"]
                .as_str()
                .unwrap_or("")
                .cmp(a["txnDate"].as_str().unwrap_or(""))
        });
        Ok(invoices)
    }

    /// Raw invoices dated `start..=end`, oldest first, paging through results.
    async fn invoices_between(&self, start: &str, end: &str) -> Result<Vec<Value>> {
        let mut all = Vec::new();
        for page in 0..MAX_PAGES {
            let payload = self
                .run_query(format!(
                    "select Id, DocNumber, TxnDate, DueDate, TotalAmt, Balance, CurrencyRef, \
                     CustomerRef from Invoice where TxnDate >= '{start}' and TxnDate <= '{end}' \
                     ORDERBY TxnDate STARTPOSITION {} MAXRESULTS {PAGE_SIZE}",
                    page * PAGE_SIZE + 1
                ))
                .await?;
            let batch = entities(&payload, "Invoice");
            let done = batch.len() < PAGE_SIZE;
            all.extend(batch);
            if done {
                break;
            }
        }
        Ok(all)
    }

    async fn class_sales_report(&self, start: &str, end: &str) -> Result<Report> {
        Ok(parse_report(
            &self
                .get(
                    "reports/ClassSales",
                    &[
                        ("accounting_method", "Accrual".to_owned()),
                        ("start_date", start.to_owned()),
                        ("end_date", end.to_owned()),
                    ],
                )
                .await?,
        ))
    }

    async fn classes(&self) -> Result<Vec<QboClass>> {
        let payload = self
            .run_query("select * from Class MAXRESULTS 1000".to_owned())
            .await?;
        Ok(resolve_parents(
            entities(&payload, "Class")
                .iter()
                .filter_map(QboClass::parse)
                .collect(),
        ))
    }
}

/// reqwest is built without a default TLS crypto provider; use ring, which
/// sqlx's rustls already links (a no-op when one is installed).
fn install_crypto_provider() {
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        let _ = rustls::crypto::ring::default_provider().install_default();
    }
}

// --- tool catalogue --------------------------------------------------------------

struct ToolSpec {
    name: &'static str,
    admin_only: bool,
    description: &'static str,
    properties: &'static str,
    required: &'static [&'static str],
}

const DATE_RANGE: &str = r#"{
    "start_date": {"type": "string", "description": "first day, YYYY-MM-DD"},
    "end_date": {"type": "string", "description": "last day, YYYY-MM-DD (inclusive)"}
}"#;

const TOOLS: &[ToolSpec] = &[
    ToolSpec {
        name: "search_customers",
        admin_only: false,
        description: "Find QuickBooks customers whose display name contains `name` (up to 25). \
            Each result lists the CRM customers (`customers.id`) linked to it through \
            `customers.qbo_id`.",
        properties: r#"{"name": {"type": "string", "description": "part of the customer's name"}}"#,
        required: &["name"],
    },
    ToolSpec {
        name: "customer_invoices",
        admin_only: false,
        description: "All QuickBooks invoices of one customer, newest first, with number, dates, \
            total, open balance and project address. Pass `customer_id` (a CRM `customers.id`; \
            if it is not linked yet it is matched in QuickBooks by email, then name) or \
            `qbo_customer_id`.",
        properties: r#"{
            "customer_id": {"type": "integer", "description": "CRM customers.id"},
            "qbo_customer_id": {"type": "string", "description": "QuickBooks customer Id"}
        }"#,
        required: &[],
    },
    ToolSpec {
        name: "invoice",
        admin_only: false,
        description: "One QuickBooks invoice in full: line items, amounts, taxes, balance, due \
            date, addresses, memo, email status and linked payments.",
        properties: r#"{"invoice_id": {"type": "string", "description": "QuickBooks invoice Id (not the DocNumber)"}}"#,
        required: &["invoice_id"],
    },
    ToolSpec {
        name: "sales_rep",
        admin_only: false,
        description: "A sales rep's QuickBooks sales for a date range, the way the app's sales \
            goal widget computes them: the ClassSales amount of the classes named after the rep, \
            and the customers (with their invoices) behind it. `rep_name` defaults to the \
            asking user; only admins may name another rep.",
        properties: r#"{
            "start_date": {"type": "string", "description": "first day, YYYY-MM-DD"},
            "end_date": {"type": "string", "description": "last day, YYYY-MM-DD (inclusive)"},
            "rep_name": {"type": "string", "description": "sales rep name as in users.name"}
        }"#,
        required: &["start_date", "end_date"],
    },
    ToolSpec {
        name: "invoices_in_range",
        admin_only: true,
        description: "Every invoice dated within a range (oldest first) with customer, total and \
            open balance, plus the count and sums. Admins only.",
        properties: DATE_RANGE,
        required: &["start_date", "end_date"],
    },
    ToolSpec {
        name: "class_sales",
        admin_only: true,
        description: "The QuickBooks Sales by Class report (sales per sales rep class) for a \
            date range, without zero rows. Admins only.",
        properties: DATE_RANGE,
        required: &["start_date", "end_date"],
    },
    ToolSpec {
        name: "query",
        admin_only: true,
        description: "Run a read-only QuickBooks query-language statement and return the raw \
            QueryResponse, e.g. `select * from Invoice where Balance > '0' ORDERBY DueDate \
            MAXRESULTS 100`, `select * from Payment where TxnDate >= '2026-01-01'`, `select \
            count(*) from Invoice`. Entities: Invoice, Payment, Customer, Estimate, SalesReceipt, \
            CreditMemo, Item, Class, Bill, Vendor, ... Admins only.",
        properties: r#"{"query": {"type": "string", "description": "a SELECT statement"}}"#,
        required: &["query"],
    },
    ToolSpec {
        name: "report",
        admin_only: true,
        description: "Fetch any QuickBooks report as raw JSON, e.g. ProfitAndLoss, BalanceSheet, \
            AgedReceivables, AgedReceivableDetail, CustomerSales, CustomerBalance, ItemSales, \
            ClassSales, TransactionList. `params` are the report's query parameters, e.g. \
            {\"start_date\": \"2026-01-01\", \"end_date\": \"2026-03-31\", \
            \"accounting_method\": \"Accrual\"}. Admins only.",
        properties: r#"{
            "report": {"type": "string", "description": "report name, e.g. ProfitAndLoss"},
            "params": {"type": "object", "description": "report query parameters"}
        }"#,
        required: &["report"],
    },
];

/// MCP descriptions of the tools this requester may use, named `qbo_<tool>`.
///
/// # Panics
///
/// Never in practice: the schemas are constants covered by the tests.
#[must_use]
pub fn tools(is_admin: bool) -> Vec<Value> {
    TOOLS
        .iter()
        .filter(|tool| is_admin || !tool.admin_only)
        .map(|tool| {
            let properties: Value =
                serde_json::from_str(tool.properties).expect("tool schemas are valid JSON");
            json!({
                "name": format!("qbo_{}", tool.name),
                "description": tool.description,
                "inputSchema": {
                    "type": "object",
                    "properties": properties,
                    "required": tool.required,
                },
            })
        })
        .collect()
}

// --- database ------------------------------------------------------------------

/// CRM customers of `company_id` linked to each QuickBooks customer id.
async fn linked_crm_customers(
    conn: &mut MySqlConnection,
    company_id: i64,
    qbo_ids: &[&str],
) -> Result<BTreeMap<String, Vec<Value>>> {
    let ids: Vec<i64> = qbo_ids
        .iter()
        .filter(|id| is_entity_id(id))
        .filter_map(|id| id.parse().ok())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let mut linked: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    if ids.is_empty() {
        return Ok(linked);
    }
    let sql = format!(
        "SELECT CAST(id AS SIGNED), CAST(name AS CHAR), CAST(qbo_id AS SIGNED) FROM customers \
          WHERE company_id = ? AND deleted_at IS NULL AND qbo_id IN ({})",
        vec!["?"; ids.len()].join(", ")
    );
    // Only placeholders are interpolated.
    let mut query =
        sqlx::query_as::<_, (i64, Option<String>, i64)>(AssertSqlSafe(sql)).bind(company_id);
    for id in ids {
        query = query.bind(id);
    }
    for (id, name, qbo_id) in query.fetch_all(conn).await? {
        linked
            .entry(qbo_id.to_string())
            .or_default()
            .push(json!({ "customerId": id, "name": name }));
    }
    Ok(linked)
}

// --- arguments -----------------------------------------------------------------

fn optional_str(args: &Value, name: &str) -> Option<String> {
    args.get(name)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn required_str(args: &Value, name: &str) -> Result<String> {
    optional_str(args, name).ok_or_else(|| Error::InvalidArgument(format!("{name} is required")))
}

fn required_date(args: &Value, name: &str) -> Result<String> {
    let value = required_str(args, name)?;
    let bytes = value.as_bytes();
    let valid = bytes.len() == 10
        && bytes.iter().enumerate().all(|(i, b)| match i {
            4 | 7 => *b == b'-',
            _ => b.is_ascii_digit(),
        });
    if valid {
        Ok(value)
    } else {
        Err(Error::InvalidArgument(format!("{name} must be YYYY-MM-DD")))
    }
}

fn optional_entity_id(args: &Value, name: &str) -> Result<Option<String>> {
    let raw = match args.get(name) {
        None | Some(Value::Null) => return Ok(None),
        Some(Value::String(text)) => text.trim().to_owned(),
        Some(Value::Number(number)) => number.to_string(),
        Some(_) => String::new(),
    };
    if is_entity_id(&raw) {
        Ok(Some(raw))
    } else {
        Err(Error::InvalidArgument(format!(
            "{name} must be a numeric QuickBooks id"
        )))
    }
}

fn optional_positive(args: &Value, name: &str) -> Result<Option<i64>> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_i64()
            .or_else(|| value.as_str().and_then(|text| text.trim().parse().ok()))
            .filter(|id| *id > 0)
            .map(Some)
            .ok_or_else(|| Error::InvalidArgument(format!("{name} must be a positive integer"))),
    }
}

/// The app's `isValidQboEntityId`.
fn is_entity_id(value: &str) -> bool {
    (1..=32).contains(&value.len()) && value.bytes().all(|b| b.is_ascii_digit())
}

/// Quotes a value for a QuickBooks query string literal, as the app does.
fn escape(value: &str) -> String {
    value.replace('\'', "''")
}

fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(char::from(byte));
            }
            other => {
                let _ = write!(out, "%{other:02X}");
            }
        }
    }
    out
}

// --- QuickBooks entities -------------------------------------------------------

/// Non-blank string, or a finite number as text (`readQboStringField`).
fn text(value: &Value) -> Option<String> {
    match value {
        Value::String(s) if !s.trim().is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// A number, also from numeric text with thousands separators.
fn number(value: &Value) -> Option<f64> {
    match value {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s
            .trim()
            .replace(',', "")
            .parse()
            .ok()
            .filter(|n: &f64| n.is_finite()),
        _ => None,
    }
}

/// `QueryResponse[key]` as a list.
fn entities(payload: &Value, key: &str) -> Vec<Value> {
    match &payload["QueryResponse"][key] {
        Value::Array(items) => items.clone(),
        Value::Object(_) => vec![payload["QueryResponse"][key].clone()],
        _ => Vec::new(),
    }
}

#[derive(Debug, Clone, PartialEq)]
struct QboCustomer {
    id: String,
    display_name: String,
    email: Option<String>,
}

impl QboCustomer {
    fn parse(value: &Value) -> Option<Self> {
        let id = text(&value["Id"])?;
        Some(Self {
            display_name: text(&value["DisplayName"])
                .unwrap_or_else(|| format!("QuickBooks customer {id}")),
            email: text(&value["PrimaryEmailAddr"]["Address"]),
            id,
        })
    }
}

/// The app's `pickQboCustomerByName`.
fn pick_by_name(matches: &[QboCustomer], name: &str, email: &str) -> Option<String> {
    let key = |value: &str| value.trim().to_lowercase();
    let name_key = key(name);
    let email_key = key(email);
    let same_email = |c: &QboCustomer| {
        !email_key.is_empty() && c.email.as_deref().map(key).as_deref() == Some(email_key.as_str())
    };

    let exact: Vec<&QboCustomer> = matches
        .iter()
        .filter(|c| key(&c.display_name) == name_key)
        .collect();
    if let [only] = exact.as_slice() {
        return Some(only.id.clone());
    }
    if exact.len() > 1
        && let [only] = exact
            .iter()
            .filter(|c| same_email(c))
            .collect::<Vec<_>>()
            .as_slice()
    {
        return Some(only.id.clone());
    }
    if let [only] = matches
        .iter()
        .filter(|c| same_email(c))
        .collect::<Vec<_>>()
        .as_slice()
    {
        return Some(only.id.clone());
    }
    if let [only] = matches {
        return Some(only.id.clone());
    }
    None
}

fn address(value: &Value) -> Option<String> {
    let parts: Vec<String> = [
        "Line1",
        "Line2",
        "Line3",
        "City",
        "CountrySubDivisionCode",
        "PostalCode",
    ]
    .iter()
    .filter_map(|field| text(&value[*field]))
    .collect();
    (!parts.is_empty()).then(|| parts.join(", "))
}

/// Compact view of an invoice (the app's `QboInvoiceSummary` plus customer).
fn invoice_summary(invoice: &Value) -> Value {
    json!({
        "id": text(&invoice["Id"]),
        "docNumber": text(&invoice["DocNumber"]),
        "txnDate": text(&invoice["TxnDate"]),
        "dueDate": text(&invoice["DueDate"]),
        "totalAmt": number(&invoice["TotalAmt"]),
        "balance": number(&invoice["Balance"]),
        "currency": text(&invoice["CurrencyRef"]["value"]),
        "qboCustomerId": text(&invoice["CustomerRef"]["value"]),
        "customerName": text(&invoice["CustomerRef"]["name"]),
        "projectAddress": address(&invoice["ShipAddr"]),
    })
}

#[derive(Debug, Clone, PartialEq)]
struct QboClass {
    id: String,
    name: String,
    fully_qualified_name: String,
    parent_id: Option<String>,
}

impl QboClass {
    fn parse(value: &Value) -> Option<Self> {
        let id = text(&value["Id"])?;
        let name = text(&value["Name"])?;
        Some(Self {
            fully_qualified_name: text(&value["FullyQualifiedName"])
                .unwrap_or_else(|| name.clone()),
            parent_id: text(&value["ParentRef"]["value"]),
            id,
            name,
        })
    }
}

/// The app's `resolveQboClassParentIds`: infer missing parents from
/// `Parent:Child` names.
fn resolve_parents(classes: Vec<QboClass>) -> Vec<QboClass> {
    let by_name: BTreeMap<String, String> = classes
        .iter()
        .map(|c| (c.fully_qualified_name.trim().to_lowercase(), c.id.clone()))
        .collect();
    classes
        .into_iter()
        .map(|mut cls| {
            if cls.parent_id.is_none()
                && let Some(separator) = cls.fully_qualified_name.rfind(':').filter(|i| *i > 0)
            {
                let parent = cls.fully_qualified_name[..separator].trim().to_lowercase();
                if let Some(parent_id) = by_name.get(&parent).filter(|id| **id != cls.id) {
                    cls.parent_id = Some(parent_id.clone());
                }
            }
            cls
        })
        .collect()
}

// --- reports -------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RowKind {
    Section,
    Data,
    SectionTotal,
    GrandTotal,
}

impl RowKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Section => "section",
            Self::Data => "data",
            Self::SectionTotal => "section_total",
            Self::GrandTotal => "grand_total",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
struct Row {
    name: String,
    amount: Option<f64>,
    depth: usize,
    kind: RowKind,
    entity_id: Option<String>,
}

impl Row {
    fn to_json(&self) -> Value {
        let mut out = Map::new();
        out.insert("name".into(), json!(self.name));
        out.insert("amount".into(), json!(self.amount));
        out.insert("depth".into(), json!(self.depth));
        out.insert("kind".into(), json!(self.kind.as_str()));
        if let Some(id) = &self.entity_id {
            out.insert("entityId".into(), json!(id));
        }
        Value::Object(out)
    }

    fn is_zero(&self) -> bool {
        self.amount.is_none_or(|amount| amount == 0.0)
    }

    fn is_not_specified(&self) -> bool {
        self.kind == RowKind::Data && self.name.trim().eq_ignore_ascii_case("not specified")
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
struct Report {
    rows: Vec<Row>,
    total: Option<f64>,
    start_period: Option<String>,
    end_period: Option<String>,
    currency: Option<String>,
}

/// The app's `parseQboClassSalesReport`: the report's rows flattened with
/// their nesting depth.
fn parse_report(payload: &Value) -> Report {
    let header = &payload["Header"];
    let mut rows = Vec::new();
    parse_rows(&payload["Rows"]["Row"], 0, &mut rows);
    Report {
        total: rows
            .iter()
            .rev()
            .find(|r| r.kind == RowKind::GrandTotal)
            .and_then(|r| r.amount),
        rows,
        start_period: text(&header["StartPeriod"]),
        end_period: text(&header["EndPeriod"]),
        currency: text(&header["Currency"]),
    }
}

fn list(value: &Value) -> Vec<&Value> {
    match value {
        Value::Array(items) => items.iter().collect(),
        Value::Null => Vec::new(),
        other => vec![other],
    }
}

/// Name, amount and entity id from the first two `ColData` cells.
fn named_amount(holder: &Value) -> Option<(String, Option<f64>, Option<String>)> {
    let cols = list(&holder["ColData"]);
    let first = cols.first()?;
    let name = text(&first["value"])?.trim().to_owned();
    let amount = cols.get(1).and_then(|col| number(&col["value"]));
    Some((name, amount, text(&first["id"])))
}

fn parse_rows(value: &Value, depth: usize, out: &mut Vec<Row>) {
    let push = |out: &mut Vec<Row>, holder: &Value, depth: usize, kind: RowKind| {
        if let Some((name, amount, entity_id)) = named_amount(holder) {
            let entity_id = (kind == RowKind::Data).then_some(entity_id).flatten();
            out.push(Row {
                name,
                amount,
                depth,
                kind,
                entity_id,
            });
        }
    };
    for row in list(value) {
        if !row.is_object() {
            continue;
        }
        if row["group"].as_str() == Some("GrandTotal") {
            push(out, &row["Summary"], depth, RowKind::GrandTotal);
            continue;
        }
        let has_header = row.get("Header").is_some();
        if has_header {
            push(out, &row["Header"], depth, RowKind::Section);
        }
        if row["Rows"].get("Row").is_some() {
            parse_rows(
                &row["Rows"]["Row"],
                if has_header { depth + 1 } else { depth },
                out,
            );
        } else if row.get("ColData").is_some() {
            push(out, row, depth, RowKind::Data);
        }
        if row.get("Summary").is_some() {
            push(out, &row["Summary"], depth, RowKind::SectionTotal);
        }
    }
}

/// The app's `omitZeroQboClassSalesRows`: drops zero data rows and totals,
/// and sections left without data.
fn omit_zero_rows(rows: &[Row]) -> Vec<Row> {
    let mut result = Vec::new();
    let mut not_specified = None;
    let mut grand_total = None;
    let mut index = 0;
    while index < rows.len() {
        let row = &rows[index];
        index += 1;
        match row.kind {
            RowKind::GrandTotal => grand_total = Some(row.clone()),
            _ if row.is_not_specified() => not_specified = Some(row.clone()),
            RowKind::Section => {
                let mut chunk = vec![row.clone()];
                while let Some(next) = rows.get(index) {
                    if matches!(next.kind, RowKind::Section | RowKind::GrandTotal)
                        || next.is_not_specified()
                    {
                        break;
                    }
                    index += 1;
                    if next.kind == RowKind::Section || !next.is_zero() {
                        chunk.push(next.clone());
                    }
                    if next.kind == RowKind::SectionTotal {
                        break;
                    }
                }
                if chunk.iter().any(|r| r.kind == RowKind::Data) {
                    result.extend(chunk);
                }
            }
            RowKind::Data if !row.is_zero() => result.push(row.clone()),
            _ => {}
        }
    }
    result.extend(not_specified.filter(|row| !row.is_zero()));
    result.extend(grand_total);
    result
}

/// The app's `rowMatchesSalesRepName`: the full name, `name/...`, or the
/// first name (3+ letters) alone or as `first/...`.
fn matches_rep(row_name: &str, rep_name: &str) -> bool {
    let row = row_name.trim().to_lowercase();
    let rep = rep_name.trim().to_lowercase();
    if row.is_empty() || rep.is_empty() {
        return false;
    }
    if row == rep || row.starts_with(&format!("{rep}/")) {
        return true;
    }
    let first = rep.split_whitespace().next().unwrap_or_default();
    first.chars().count() >= 3 && (row == first || row.starts_with(&format!("{first}/")))
}

/// Sum of `amounts`, `0.0` when empty (`Iterator::sum` gives `-0.0`).
fn total(amounts: impl Iterator<Item = f64>) -> f64 {
    amounts.fold(0.0, |sum, amount| sum + amount)
}

/// The app's `sumQboClassSalesForRepName`.
fn sum_for_rep(rows: &[Row], rep_name: &str) -> f64 {
    total(
        rows.iter()
            .filter(|row| row.kind == RowKind::Data && matches_rep(&row.name, rep_name))
            .filter_map(|row| row.amount),
    )
}

/// The app's `classIdsMatchingSalesRepName`: matching classes and all
/// their descendants.
fn class_ids_for_rep(classes: &[QboClass], rep_name: &str) -> Vec<String> {
    if rep_name.trim().is_empty() {
        return Vec::new();
    }
    let mut matched: Vec<String> = classes
        .iter()
        .filter(|cls| {
            matches_rep(&cls.name, rep_name)
                || matches_rep(&cls.fully_qualified_name, rep_name)
                || cls
                    .fully_qualified_name
                    .rsplit(':')
                    .next()
                    .is_some_and(|leaf| matches_rep(leaf, rep_name))
        })
        .map(|cls| cls.id.clone())
        .collect();
    loop {
        let children: Vec<String> = classes
            .iter()
            .filter(|cls| {
                cls.parent_id
                    .as_ref()
                    .is_some_and(|parent| matched.contains(parent))
                    && !matched.contains(&cls.id)
            })
            .map(|cls| cls.id.clone())
            .collect();
        if children.is_empty() {
            return matched;
        }
        matched.extend(children);
    }
}

#[derive(Debug, Clone, PartialEq)]
struct CustomerSales {
    name: String,
    amount: f64,
    qbo_customer_id: Option<String>,
    invoices: Vec<Value>,
}

impl CustomerSales {
    fn to_json(&self) -> Value {
        json!({
            "name": self.name,
            "amount": self.amount,
            "qboCustomerId": self.qbo_customer_id,
            "invoices": self.invoices,
        })
    }
}

/// The app's `customerSalesRowsFromReport`: non-zero customers, largest first.
fn customer_sales_rows(report: &Report) -> Vec<CustomerSales> {
    let mut rows: Vec<CustomerSales> = report
        .rows
        .iter()
        .filter(|row| row.kind == RowKind::Data)
        .filter_map(|row| {
            let amount = row.amount.filter(|amount| *amount != 0.0)?;
            Some(CustomerSales {
                name: row.name.clone(),
                amount,
                qbo_customer_id: row.entity_id.clone(),
                invoices: Vec::new(),
            })
        })
        .collect();
    rows.sort_by(|a, b| {
        b.amount
            .total_cmp(&a.amount)
            .then_with(|| a.name.cmp(&b.name))
    });
    rows
}

/// Orders dates ascending with missing ones last.
fn by_date(a: &str, b: &str) -> std::cmp::Ordering {
    match (a.is_empty(), b.is_empty()) {
        (true, false) => std::cmp::Ordering::Greater,
        (false, true) => std::cmp::Ordering::Less,
        _ => a.cmp(b),
    }
}

/// The app's `attachInvoicesToCustomerSales`: each customer's invoices in
/// the range (by id, else by name), customers ordered by first invoice date.
fn attach_invoices(customers: Vec<CustomerSales>, invoices: &[Value]) -> Vec<CustomerSales> {
    let mut customers: Vec<CustomerSales> = customers
        .into_iter()
        .map(|mut customer| {
            let mut matched: Vec<Value> = invoices
                .iter()
                .filter(|invoice| {
                    let id = text(&invoice["CustomerRef"]["value"]);
                    let name = text(&invoice["CustomerRef"]["name"]);
                    (customer.qbo_customer_id.is_some() && id == customer.qbo_customer_id)
                        || name.is_some_and(|name| {
                            name.trim().eq_ignore_ascii_case(customer.name.trim())
                        })
                })
                .map(|invoice| {
                    json!({
                        "id": text(&invoice["Id"]),
                        "docNumber": text(&invoice["DocNumber"]),
                        "totalAmt": number(&invoice["TotalAmt"]),
                        "balance": number(&invoice["Balance"]),
                        "txnDate": text(&invoice["TxnDate"]),
                    })
                })
                .collect();
            matched.sort_by(|a, b| {
                by_date(
                    a["txnDate"].as_str().unwrap_or(""),
                    b["txnDate"].as_str().unwrap_or(""),
                )
                .then_with(|| {
                    let doc = |v: &Value| v["docNumber"].as_str().unwrap_or("").to_owned();
                    natural(&doc(a), &doc(b))
                })
            });
            customer.invoices = matched;
            customer
        })
        .collect();
    customers.sort_by(|a, b| {
        let first = |c: &CustomerSales| {
            c.invoices
                .first()
                .and_then(|i| i["txnDate"].as_str())
                .unwrap_or("")
                .to_owned()
        };
        by_date(&first(a), &first(b)).then_with(|| a.name.cmp(&b.name))
    });
    customers
}

/// Numeric-aware comparison for document numbers ("9" before "10").
fn natural(a: &str, b: &str) -> std::cmp::Ordering {
    match (a.parse::<u64>(), b.parse::<u64>()) {
        (Ok(x), Ok(y)) => x.cmp(&y),
        _ => a.cmp(b),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn class_sales_payload() -> Value {
        json!({
            "Header": {"StartPeriod": "2026-09-01", "EndPeriod": "2026-09-27", "Currency": "USD"},
            "Rows": {"Row": [
                {
                    "Header": {"ColData": [{"value": "Sales"}, {"value": ""}]},
                    "Rows": {"Row": [
                        {"ColData": [{"value": "Alina", "id": "5"}, {"value": "1,200.50"}]},
                        {"ColData": [{"value": "Alina/Kitchens", "id": "6"}, {"value": "300"}]},
                        {"ColData": [{"value": "Bob", "id": "7"}, {"value": "0"}]}
                    ]},
                    "Summary": {"ColData": [{"value": "Total for Sales"}, {"value": "1500.50"}]}
                },
                {
                    "Header": {"ColData": [{"value": "Empty"}, {"value": ""}]},
                    "Rows": {"Row": [{"ColData": [{"value": "Zed"}, {"value": "0"}]}]},
                    "Summary": {"ColData": [{"value": "Total for Empty"}, {"value": "0"}]}
                },
                {"ColData": [{"value": "Not Specified"}, {"value": "25"}]},
                {"group": "GrandTotal", "Summary": {"ColData": [{"value": "TOTAL"}, {"value": "1525.50"}]}}
            ]}
        })
    }

    #[test]
    fn reports_are_flattened_with_depth_and_totals() {
        let report = parse_report(&class_sales_payload());
        assert_eq!(report.total, Some(1525.5));
        assert_eq!(report.start_period.as_deref(), Some("2026-09-01"));
        assert_eq!(report.currency.as_deref(), Some("USD"));
        let kinds: Vec<_> = report
            .rows
            .iter()
            .map(|r| (r.name.as_str(), r.depth, r.kind))
            .collect();
        assert_eq!(kinds[0], ("Sales", 0, RowKind::Section));
        assert_eq!(kinds[1], ("Alina", 1, RowKind::Data));
        assert_eq!(report.rows[1].amount, Some(1200.5));
        assert_eq!(report.rows[1].entity_id.as_deref(), Some("5"));
        assert_eq!(kinds[4], ("Total for Sales", 0, RowKind::SectionTotal));
        assert_eq!(kinds.last().unwrap(), &("TOTAL", 0, RowKind::GrandTotal));
    }

    #[test]
    fn zero_rows_and_empty_sections_are_omitted() {
        let rows = omit_zero_rows(&parse_report(&class_sales_payload()).rows);
        let names: Vec<_> = rows.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "Sales",
                "Alina",
                "Alina/Kitchens",
                "Total for Sales",
                "Not Specified",
                "TOTAL"
            ]
        );
    }

    #[test]
    fn rep_names_match_like_the_app() {
        assert!(matches_rep("Alina", "alina"));
        assert!(matches_rep("Alina/Kitchens", "Alina"));
        assert!(matches_rep("alina", "Alina Petrova"));
        assert!(matches_rep("Alina/2026", "Alina Petrova"));
        assert!(!matches_rep("Alinas", "Alina"));
        assert!(!matches_rep("Al", "Al Bundy"));
        let rows = parse_report(&class_sales_payload()).rows;
        assert!((sum_for_rep(&rows, "Alina Petrova") - 1500.5).abs() < 1e-9);
        assert!(
            sum_for_rep(&rows, "").to_bits() == 0.0f64.to_bits(),
            "not -0.0"
        );
    }

    #[test]
    fn rep_classes_include_descendants_and_inferred_parents() {
        let class = |id: &str, name: &str, fqn: &str, parent: Option<&str>| QboClass {
            id: id.to_owned(),
            name: name.to_owned(),
            fully_qualified_name: fqn.to_owned(),
            parent_id: parent.map(str::to_owned),
        };
        let classes = resolve_parents(vec![
            class("1", "Alina", "Alina", None),
            class("2", "Kitchens", "Alina:Kitchens", None),
            class("3", "Baths", "Alina:Kitchens:Baths", Some("2")),
            class("4", "Bob", "Bob", None),
        ]);
        assert_eq!(classes[1].parent_id.as_deref(), Some("1"));
        let mut ids = class_ids_for_rep(&classes, "Alina");
        ids.sort();
        assert_eq!(ids, ["1", "2", "3"]);
    }

    #[test]
    fn customer_sales_get_their_invoices() {
        let report = Report {
            rows: vec![
                Row {
                    name: "Acme".into(),
                    amount: Some(100.0),
                    depth: 0,
                    kind: RowKind::Data,
                    entity_id: Some("9".into()),
                },
                Row {
                    name: "Zero".into(),
                    amount: Some(0.0),
                    depth: 0,
                    kind: RowKind::Data,
                    entity_id: None,
                },
                Row {
                    name: "Beta LLC".into(),
                    amount: Some(250.0),
                    depth: 0,
                    kind: RowKind::Data,
                    entity_id: None,
                },
            ],
            ..Report::default()
        };
        let customers = customer_sales_rows(&report);
        assert_eq!(
            customers
                .iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>(),
            ["Beta LLC", "Acme"]
        );

        let invoices = vec![
            json!({"Id": "1", "DocNumber": "10", "TxnDate": "2026-09-05", "TotalAmt": 60, "CustomerRef": {"value": "9", "name": "Acme"}}),
            json!({"Id": "2", "DocNumber": "9", "TxnDate": "2026-09-05", "TotalAmt": 40, "CustomerRef": {"value": "9", "name": "Acme"}}),
            json!({"Id": "3", "DocNumber": "11", "TxnDate": "2026-09-10", "TotalAmt": 250, "CustomerRef": {"value": "12", "name": "beta llc"}}),
            json!({"Id": "4", "TxnDate": "2026-09-01", "CustomerRef": {"value": "99", "name": "Other"}}),
        ];
        let attached = attach_invoices(customers, &invoices);
        assert_eq!(attached[0].name, "Acme");
        let docs: Vec<_> = attached[0]
            .invoices
            .iter()
            .map(|i| i["docNumber"].clone())
            .collect();
        assert_eq!(docs, [json!("9"), json!("10")]);
        assert_eq!(attached[1].invoices[0]["id"], "3");
    }

    #[test]
    fn customers_are_picked_by_name_then_email() {
        let customer = |id: &str, name: &str, email: Option<&str>| QboCustomer {
            id: id.to_owned(),
            display_name: name.to_owned(),
            email: email.map(str::to_owned),
        };
        let matches = vec![
            customer("1", "John Smith", Some("a@x.com")),
            customer("2", "John Smith", Some("b@x.com")),
            customer("3", "John Smithers", None),
        ];
        assert_eq!(
            pick_by_name(&matches, "john smith", "B@x.com").as_deref(),
            Some("2")
        );
        assert_eq!(pick_by_name(&matches, "John Smith", ""), None);
        assert_eq!(
            pick_by_name(&matches, "John Smithers", "").as_deref(),
            Some("3")
        );
        assert_eq!(
            pick_by_name(&matches[2..], "Someone", "").as_deref(),
            Some("3")
        );
    }

    #[test]
    fn invoices_are_summarised() {
        let summary = invoice_summary(&json!({
            "Id": "42", "DocNumber": "1001", "TxnDate": "2026-09-01", "TotalAmt": 1200.5,
            "Balance": "200", "CurrencyRef": {"value": "USD"},
            "CustomerRef": {"value": "9", "name": "Acme"},
            "ShipAddr": {"Line1": "1 Main St", "City": "Columbus", "CountrySubDivisionCode": "OH"}
        }));
        assert_eq!(summary["id"], "42");
        assert_eq!(summary["balance"], 200.0);
        assert_eq!(summary["projectAddress"], "1 Main St, Columbus, OH");
        assert_eq!(summary["customerName"], "Acme");
        assert_eq!(summary["dueDate"], Value::Null);
    }

    #[test]
    fn arguments_are_validated() {
        let args = json!({"a": " 2026-09-01 ", "b": "2026-9-1", "id": 17, "sid": "0017", "bad": "1e3", "neg": -1});
        assert_eq!(required_date(&args, "a").unwrap(), "2026-09-01");
        assert!(required_date(&args, "b").is_err());
        assert!(required_date(&args, "missing").is_err());
        assert_eq!(
            optional_entity_id(&args, "id").unwrap().as_deref(),
            Some("17")
        );
        assert_eq!(
            optional_entity_id(&args, "sid").unwrap().as_deref(),
            Some("0017")
        );
        assert!(optional_entity_id(&args, "bad").is_err());
        assert_eq!(optional_entity_id(&args, "missing").unwrap(), None);
        assert_eq!(optional_positive(&args, "id").unwrap(), Some(17));
        assert!(optional_positive(&args, "neg").is_err());
    }

    #[test]
    fn query_values_are_quoted_and_urls_encoded() {
        assert_eq!(escape("O'Brien"), "O''Brien");
        assert_eq!(
            percent_encode("select * from Invoice"),
            "select%20%2A%20from%20Invoice"
        );
    }

    #[test]
    fn admin_tools_are_hidden_from_other_users() {
        let names = |admin| {
            tools(admin)
                .into_iter()
                .map(|tool| tool["name"].as_str().unwrap().to_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            names(false),
            [
                "qbo_search_customers",
                "qbo_customer_invoices",
                "qbo_invoice",
                "qbo_sales_rep"
            ]
        );
        assert_eq!(names(true).len(), TOOLS.len());
        assert!(names(true).contains(&"qbo_query".to_owned()));
    }

    #[test]
    fn sessions_need_a_numeric_realm() {
        let scope = Scope {
            company_id: 1,
            user_id: 2,
            is_admin: false,
        };
        assert!(Session::new("https://x", "12a", "t", scope).is_err());
        let session = Session::new("https://x/", "123", "t", scope).unwrap();
        assert_eq!(session.company_url, "https://x/v3/company/123");
    }
}

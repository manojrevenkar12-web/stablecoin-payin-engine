-- The event log is the record. The engine's state is rebuilt from it; journals and postings are
-- written in the same transaction as the event that produced them, and the database refuses a
-- journal that does not balance or that would leave the reserve short of the tokens.

create table if not exists events (
    seq             bigserial primary key,
    at              bigint      not null,
    kind            text        not null,
    idempotency_key text        unique,
    body            jsonb       not null,
    recorded_at     timestamptz not null default now()
);

create table if not exists journals (
    id        bigint primary key,
    at        bigint not null,
    kind      text   not null,
    reference text   not null,
    event_seq bigint not null references events(seq)
);

create table if not exists postings (
    journal_id   bigint  not null references journals(id),
    account      text    not null,
    ccy          char(3) not null check (ccy in ('EUR', 'USD')),
    amount_minor bigint  not null check (amount_minor <> 0)
);
create index if not exists postings_account on postings (account, ccy);
create index if not exists postings_journal on postings (journal_id);

create or replace function pie_append_only() returns trigger language plpgsql as $$
begin
    raise exception 'table % is append-only', tg_table_name;
end $$;

drop trigger if exists events_append_only on events;
create trigger events_append_only before update or delete on events for each row execute function pie_append_only();
drop trigger if exists journals_append_only on journals;
create trigger journals_append_only before update or delete on journals for each row execute function pie_append_only();
drop trigger if exists postings_append_only on postings;
create trigger postings_append_only before update or delete on postings for each row execute function pie_append_only();

-- Every journal sums to zero per currency, checked when the transaction commits.
create or replace function pie_check_balanced() returns trigger language plpgsql as $$
begin
    if exists (select 1 from postings where journal_id = new.journal_id group by ccy having sum(amount_minor) <> 0) then
        raise exception 'journal % does not balance', new.journal_id;
    end if;
    return null;
end $$;
drop trigger if exists postings_balanced on postings;
create constraint trigger postings_balanced after insert on postings deferrable initially deferred
    for each row execute function pie_check_balanced();

-- The segregated reserve covers every token outstanding or being minted, checked at commit.
create or replace function pie_check_reserve() returns trigger language plpgsql as $$
declare
    reserve bigint;
    tokens  bigint;
begin
    select coalesce(sum(amount_minor), 0) into reserve from postings
        where ccy = new.ccy and account = 'bank:' || new.ccy || ':reserve';
    select coalesce(-sum(amount_minor), 0) into tokens from postings
        where ccy = new.ccy and account in ('tokens:' || new.ccy || ':outstanding', 'tokens:' || new.ccy || ':pending_mint');
    if reserve < tokens then
        raise exception '% reserve % would not cover % of tokens', new.ccy, reserve, tokens;
    end if;
    return null;
end $$;
drop trigger if exists postings_reserve on postings;
create constraint trigger postings_reserve after insert on postings deferrable initially deferred
    for each row execute function pie_check_reserve();

create or replace view balances as
    select account, ccy, sum(amount_minor)::bigint as balance_minor from postings group by account, ccy;

-- API idempotency (MoonPay Enterprise semantics: same key and fingerprint replays the stored 2xx;
-- same key, different fingerprint is 409; failures are not stored).
create table if not exists idempotency_keys (
    key         text primary key,
    fingerprint text not null,
    state       text not null check (state in ('in_flight', 'done')),
    status_code int,
    body        jsonb,
    created_at  timestamptz not null default now()
);

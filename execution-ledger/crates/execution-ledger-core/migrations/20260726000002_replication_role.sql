DO $$
BEGIN
    IF EXISTS (
        SELECT 1
        FROM pg_roles
        WHERE rolname = current_user
          AND rolreplication
    ) THEN
        RETURN;
    END IF;

    EXECUTE format('ALTER ROLE %I WITH REPLICATION', current_user);
END $$;

create table if not exists images (
                                      id bigserial primary key,
                                      guid uuid not null unique,
                                      link_o text,
                                      status int,
                                      status_date timestamptz
);

create index if not exists images_guid_idx on images(guid);
create index if not exists images_link_o_idx on images(link_o);

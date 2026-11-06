-- Common names of currencies, version 2 (production seed `currencies-iso4217-v2`).
-- Version 1 is never edited; this version only sets `common_names`, most common
-- first, and a rerun sets the same values. Ambiguous symbols ($, ¥, £) are not
-- names on their own.
UPDATE currencies c SET common_names = v.names
FROM (VALUES
('CNY', ARRAY['RMB','Renminbi','人民币']),
('MYR', ARRAY['RM','Ringgit']),
('USD', ARRAY['US$']),
('HKD', ARRAY['HK$']),
('SGD', ARRAY['S$']),
('GBP', ARRAY['Sterling','Pound sterling']),
('JPY', ARRAY['Yen','円']),
('EUR', ARRAY['Euro']),
('INR', ARRAY['Rupee']),
('IDR', ARRAY['Rupiah']),
('THB', ARRAY['Baht']),
('PHP', ARRAY['Peso']),
('KRW', ARRAY['Won']),
('TWD', ARRAY['NT$']),
('AUD', ARRAY['A$'])
) AS v(code, names)
WHERE c.code = v.code;

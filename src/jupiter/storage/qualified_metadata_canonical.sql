CREATE FUNCTION mst2_metadata_read_le(b bytea,p integer,w integer) RETURNS numeric
LANGUAGE plpgsql IMMUTABLE STRICT SET search_path=$Q_SCHEMA$,pg_catalog,pg_temp AS $$
DECLARE v numeric:=0; power numeric:=1; i integer;
BEGIN
  IF w NOT IN (2,4,8) OR p<0 OR p>octet_length(b)-w THEN
    RAISE EXCEPTION 'MTP2 integer is out of bounds';
  END IF;
  FOR i IN 0..w-1 LOOP
    v:=v+get_byte(b,p+i)*power; power:=power*256;
  END LOOP;
  RETURN v;
END $$;

CREATE FUNCTION mst2_metadata_write_le(v numeric,w integer) RETURNS bytea
LANGUAGE plpgsql IMMUTABLE STRICT SET search_path=$Q_SCHEMA$,pg_catalog,pg_temp AS $$
DECLARE result bytea; i integer;
BEGIN
  IF w NOT IN (2,4,8) OR v<0 OR v<>trunc(v) OR v>=power(256::numeric,w) THEN
    RAISE EXCEPTION 'MTP2 output integer is out of range';
  END IF;
  result:=decode(repeat('00',w),'hex');
  FOR i IN 0..w-1 LOOP result:=set_byte(result,i,mod(v,256)::integer); v:=div(v,256); END LOOP;
  RETURN result;
END $$;

CREATE FUNCTION mst2_metadata_encode_entry(e jsonb) RETURNS bytea
LANGUAGE plpgsql IMMUTABLE STRICT SET search_path=$Q_SCHEMA$,pg_catalog,pg_temp AS $$
DECLARE kind integer:=(e->>'kind')::integer; name bytea:=decode(e->>'name','hex'); target bytea; result bytea;
BEGIN
  IF kind IS NULL OR kind NOT BETWEEN 1 AND 4 OR name IS NULL THEN RAISE EXCEPTION 'MTP2 output entry is incomplete'; END IF;
  PERFORM mst2_metadata_valid_name(name);
  result:=set_byte(decode('00','hex'),0,kind)||mst2_metadata_write_le(octet_length(name),2)||name;
  IF kind=4 THEN
    target:=decode(e->>'child','hex');
    IF target IS NULL OR octet_length(target)<>32 OR target=decode(repeat('00',32),'hex') THEN
      RAISE EXCEPTION 'MTP2 output directory reference is invalid';
    END IF;
  ELSE
    target:=decode(e->>'content_id','hex');
    IF target IS NULL OR octet_length(target)<>32 OR e->>'size' IS NULL THEN
      RAISE EXCEPTION 'MTP2 output file reference is invalid';
    END IF;
    result:=result||mst2_metadata_write_le((e->>'size')::numeric,8);
  END IF;
  RETURN result||target;
END $$;

CREATE FUNCTION mst2_metadata_build_map(items jsonb,budget bigint DEFAULT 67108864) RETURNS jsonb
LANGUAGE plpgsql IMMUTABLE STRICT SET search_path=$Q_SCHEMA$,pg_catalog,pg_temp AS $$
DECLARE n integer; item jsonb; previous bytea; name bytea; minimum bytea; maximum bytea;
  raw bytea:=''::bytea; result bytea; prefix bytea; terminal jsonb;
  partition record; grouped jsonb:='[]'::jsonb; child jsonb;
  visits bigint:=0; encoded_bytes bigint:=0; pages bigint:=1; total_bytes bigint; payload bytea; children integer:=0;
BEGIN
  IF jsonb_typeof(items)<>'array' OR budget NOT BETWEEN 1 AND 67108864 THEN
    RAISE EXCEPTION 'MTP2 canonical map input or work budget is invalid';
  END IF;
  n:=jsonb_array_length(items);
  IF n>131072 THEN RAISE EXCEPTION 'MTP2 canonical map exceeds its entry budget'; END IF;
  FOR item IN SELECT value FROM jsonb_array_elements(items) LOOP
    name:=decode(item->>'name','hex');
    IF previous IS NOT NULL AND previous>=name THEN RAISE EXCEPTION 'MTP2 build names are not strictly byte ordered'; END IF;
    previous:=name; maximum:=name; IF minimum IS NULL THEN minimum:=name; END IF;
    payload:=mst2_metadata_encode_entry(item); visits:=visits+1+octet_length(payload);
    IF visits>budget THEN RAISE EXCEPTION 'MTP2 canonical source work budget exceeded'; END IF;
    encoded_bytes:=encoded_bytes+octet_length(payload);
    IF n<=128 AND 20+encoded_bytes<=16384 THEN raw:=raw||payload; END IF;
  END LOOP;
  IF n<=128 AND 20+encoded_bytes<=16384 THEN
    result:=decode('4d5450320000','hex')||mst2_metadata_write_le(n,2)||mst2_metadata_write_le(n,8)
      ||mst2_metadata_write_le(octet_length(raw),4)||raw;
    RETURN jsonb_build_object('bytes',encode(result,'hex'),'page_id',encode(sha256(
      convert_to('mega.mst2.metapage','UTF8')||decode('00','hex')||result),'hex'),
      'source_work_units',visits,'pages',pages,'metadata_bytes',octet_length(result));
  END IF;
  prefix:=mst2_metadata_lcp(minimum,maximum);
  SELECT value INTO terminal FROM jsonb_array_elements(items) WHERE decode(value->>'name','hex')=prefix;
  visits:=visits+2*n;
  IF visits>budget THEN RAISE EXCEPTION 'MTP2 canonical source grouping work budget exceeded'; END IF;
  payload:=mst2_metadata_write_le(octet_length(prefix),2)||prefix;
  IF terminal IS NULL THEN payload:=payload||decode('00','hex');
  ELSE payload:=payload||decode('01','hex')||mst2_metadata_encode_entry(terminal); END IF;
  total_bytes:=0;
  FOR partition IN SELECT get_byte(decode(value->>'name','hex'),octet_length(prefix)) AS label,
      jsonb_agg(value ORDER BY decode(value->>'name','hex')) AS members
      FROM jsonb_array_elements(items) WHERE octet_length(decode(value->>'name','hex'))>octet_length(prefix)
      GROUP BY get_byte(decode(value->>'name','hex'),octet_length(prefix)) ORDER BY label LOOP
    grouped:=partition.members;
    IF visits>=budget THEN RAISE EXCEPTION 'MTP2 canonical source work budget exceeded'; END IF;
    child:=mst2_metadata_build_map(grouped,budget-visits); visits:=visits+(child->>'source_work_units')::bigint;
    pages:=pages+(child->>'pages')::bigint; total_bytes:=total_bytes+(child->>'metadata_bytes')::bigint;
    IF pages>4096 OR total_bytes>67108864 THEN RAISE EXCEPTION 'MTP2 canonical source encoding exceeds metadata budget'; END IF;
    payload:=payload||set_byte(decode('00','hex'),0,partition.label)||mst2_metadata_write_le(jsonb_array_length(grouped),8)
      ||decode(child->>'page_id','hex'); children:=children+1;
  END LOOP;
  IF children+(terminal IS NOT NULL)::integer<2 OR 20+octet_length(payload)>16384 THEN
    RAISE EXCEPTION 'MTP2 canonical branch shape or size is invalid';
  END IF;
  result:=decode('4d5450320100','hex')||mst2_metadata_write_le(children,2)||mst2_metadata_write_le(n,8)
    ||mst2_metadata_write_le(octet_length(payload),4)||payload;
  total_bytes:=total_bytes+octet_length(result);
  IF total_bytes>67108864 THEN RAISE EXCEPTION 'MTP2 canonical source encoding exceeds metadata byte budget'; END IF;
  RETURN jsonb_build_object('bytes',encode(result,'hex'),'page_id',encode(sha256(
    convert_to('mega.mst2.metapage','UTF8')||decode('00','hex')||result),'hex'),
    'source_work_units',visits,'pages',pages,'metadata_bytes',total_bytes);
END $$;

CREATE FUNCTION mst2_metadata_valid_name(n bytea) RETURNS boolean
LANGUAGE plpgsql IMMUTABLE STRICT SET search_path=$Q_SCHEMA$,pg_catalog,pg_temp AS $$
DECLARE i integer;
BEGIN
  IF octet_length(n) NOT BETWEEN 1 AND 255 OR n IN (decode('2e','hex'),decode('2e2e','hex')) THEN
    RAISE EXCEPTION 'MTP2 name has an invalid length or dot component';
  END IF;
  FOR i IN 0..octet_length(n)-1 LOOP
    IF get_byte(n,i) IN (0,47) THEN RAISE EXCEPTION 'MTP2 name contains NUL or slash'; END IF;
  END LOOP;
  PERFORM convert_from(n,'UTF8');
  RETURN true;
END $$;

CREATE FUNCTION mst2_metadata_decode_entry(b bytea,p integer) RETURNS jsonb
LANGUAGE plpgsql IMMUTABLE STRICT SET search_path=$Q_SCHEMA$,pg_catalog,pg_temp AS $$
DECLARE start integer:=p; kind integer; n integer; name bytea; target bytea; size numeric;
BEGIN
  IF p<0 OR p>=octet_length(b) THEN RAISE EXCEPTION 'MTP2 entry kind is truncated'; END IF;
  kind:=get_byte(b,p); p:=p+1;
  IF kind NOT BETWEEN 1 AND 4 THEN RAISE EXCEPTION 'MTP2 entry kind is invalid'; END IF;
  n:=mst2_metadata_read_le(b,p,2)::integer; p:=p+2;
  IF n>octet_length(b)-p THEN RAISE EXCEPTION 'MTP2 entry name is truncated'; END IF;
  name:=substring(b FROM p+1 FOR n); p:=p+n;
  PERFORM mst2_metadata_valid_name(name);
  IF kind=4 THEN
    IF p>octet_length(b)-32 THEN RAISE EXCEPTION 'MTP2 directory reference is truncated'; END IF;
    target:=substring(b FROM p+1 FOR 32); p:=p+32;
    IF target=decode(repeat('00',32),'hex') THEN RAISE EXCEPTION 'MTP2 empty directory has a zero reference'; END IF;
    RETURN jsonb_build_object('end',p,'kind',kind,'name',encode(name,'hex'),
      'encoded_bytes',p-start,'child',encode(target,'hex'));
  END IF;
  size:=mst2_metadata_read_le(b,p,8); p:=p+8;
  IF p>octet_length(b)-32 THEN RAISE EXCEPTION 'MTP2 file content reference is truncated'; END IF;
  target:=substring(b FROM p+1 FOR 32); p:=p+32;
  RETURN jsonb_build_object('end',p,'kind',kind,'name',encode(name,'hex'),
    'encoded_bytes',p-start,'size',size,'content_id',encode(target,'hex'));
END $$;

CREATE FUNCTION mst2_metadata_decode_local(b bytea) RETURNS jsonb
LANGUAGE plpgsql IMMUTABLE STRICT SET search_path=$Q_SCHEMA$,pg_catalog,pg_temp AS $$
DECLARE kind integer; n integer; count numeric; declared numeric; p integer:=20; plen integer;
  i integer; label integer; previous_label integer:=-1; previous_name bytea; name bytea; prefix bytea;
  terminal integer:=0; item jsonb; entries jsonb:='[]'::jsonb; children jsonb:='[]'::jsonb;
  refs jsonb:='[]'::jsonb; child bytea; child_count numeric; entry_bytes bigint:=0;
BEGIN
  IF octet_length(b) NOT BETWEEN 20 AND 16384 THEN RAISE EXCEPTION 'MTP2 page length is invalid'; END IF;
  IF substring(b FROM 1 FOR 4)<>decode('4d545032','hex') OR get_byte(b,5)<>0 THEN
    RAISE EXCEPTION 'MTP2 magic or flags are invalid';
  END IF;
  kind:=get_byte(b,4); n:=mst2_metadata_read_le(b,6,2)::integer;
  count:=mst2_metadata_read_le(b,8,8);
  IF count>9223372036854775807 OR mst2_metadata_read_le(b,16,4)<>octet_length(b)-20 THEN
    RAISE EXCEPTION 'MTP2 header count or payload length is invalid';
  END IF;
  IF kind=0 THEN
    IF n>128 OR count<>n THEN RAISE EXCEPTION 'MTP2 leaf count is invalid'; END IF;
    IF n>0 THEN
      FOR i IN 1..n LOOP
        item:=mst2_metadata_decode_entry(b,p); p:=(item->>'end')::integer;
        name:=decode(item->>'name','hex');
        IF previous_name IS NOT NULL AND previous_name>=name THEN
          RAISE EXCEPTION 'MTP2 leaf names are not strictly byte ordered';
        END IF;
        previous_name:=name; entries:=entries||jsonb_build_array(item-'end');
        entry_bytes:=entry_bytes+(item->>'encoded_bytes')::bigint;
        IF (item->>'kind')::integer=4 THEN
          refs:=refs||jsonb_build_array(jsonb_build_object('kind','DIRECTORY','name',item->>'name','child',item->>'child'));
        END IF;
      END LOOP;
    END IF;
  ELSIF kind=1 THEN
    plen:=mst2_metadata_read_le(b,p,2)::integer; p:=p+2;
    IF plen>octet_length(b)-p THEN RAISE EXCEPTION 'MTP2 branch prefix is truncated'; END IF;
    prefix:=substring(b FROM p+1 FOR plen); p:=p+plen;
    IF p>=octet_length(b) THEN RAISE EXCEPTION 'MTP2 terminal flag is truncated'; END IF;
    terminal:=get_byte(b,p); p:=p+1;
    IF terminal NOT IN (0,1) THEN RAISE EXCEPTION 'MTP2 terminal flag is invalid'; END IF;
    IF terminal=1 THEN
      item:=mst2_metadata_decode_entry(b,p); p:=(item->>'end')::integer;
      IF decode(item->>'name','hex')<>prefix THEN RAISE EXCEPTION 'MTP2 terminal name differs from prefix'; END IF;
      entries:=entries||jsonb_build_array(item-'end'); entry_bytes:=(item->>'encoded_bytes')::bigint;
      IF (item->>'kind')::integer=4 THEN
        refs:=refs||jsonb_build_array(jsonb_build_object('kind','DIRECTORY','name',item->>'name','child',item->>'child'));
      END IF;
    END IF;
    IF n>256 OR n+terminal<2 THEN RAISE EXCEPTION 'MTP2 branch group count is invalid'; END IF;
    declared:=terminal;
    IF n>0 THEN
      FOR i IN 1..n LOOP
        IF p>=octet_length(b) THEN RAISE EXCEPTION 'MTP2 branch child label is truncated'; END IF;
        label:=get_byte(b,p); p:=p+1;
        IF label<=previous_label THEN RAISE EXCEPTION 'MTP2 branch labels are not strictly ordered'; END IF;
        previous_label:=label; child_count:=mst2_metadata_read_le(b,p,8); p:=p+8;
        IF child_count NOT BETWEEN 1 AND 9223372036854775807 THEN RAISE EXCEPTION 'MTP2 child count is invalid'; END IF;
        IF p>octet_length(b)-32 THEN RAISE EXCEPTION 'MTP2 branch child digest is truncated'; END IF;
        child:=substring(b FROM p+1 FOR 32); p:=p+32;
        declared:=declared+child_count;
        IF declared>9223372036854775807 THEN RAISE EXCEPTION 'MTP2 branch count overflows'; END IF;
        item:=jsonb_build_object('kind','RADIX','label',label,'count',child_count,'child',encode(child,'hex'));
        children:=children||jsonb_build_array(item); refs:=refs||jsonb_build_array(item);
      END LOOP;
    END IF;
    IF declared<>count THEN RAISE EXCEPTION 'MTP2 branch header count differs from children'; END IF;
  ELSE
    RAISE EXCEPTION 'MTP2 page kind is invalid';
  END IF;
  IF p<>octet_length(b) THEN RAISE EXCEPTION 'MTP2 page has trailing bytes'; END IF;
  RETURN jsonb_build_object('kind',kind,'count',count,'prefix',encode(prefix,'hex'),
    'entries',entries,'children',children,'refs',refs,'direct_entry_bytes',entry_bytes,
    'page_id',encode(sha256(convert_to('mega.mst2.metapage','UTF8')||decode('00','hex')||b),'hex'));
END $$;

CREATE FUNCTION mst2_metadata_lcp(a bytea,b bytea) RETURNS bytea
LANGUAGE plpgsql IMMUTABLE STRICT SET search_path=$Q_SCHEMA$,pg_catalog,pg_temp AS $$
DECLARE n integer:=least(octet_length(a),octet_length(b)); i integer:=0;
BEGIN
  WHILE i<n AND get_byte(a,i)=get_byte(b,i) LOOP i:=i+1; END LOOP;
  RETURN substring(a FROM 1 FOR i);
END $$;
